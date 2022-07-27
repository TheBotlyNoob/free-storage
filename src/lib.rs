use std::mem::MaybeUninit;

use reqwest::{header, Client, Url};
use serde::{Deserialize, Serialize};

#[derive(Debug)]
pub enum Error {
    Reqwest(reqwest::Error),
    Serde(serde_json::Error),
    Io(std::io::Error),
    Tokio(tokio::task::JoinError),
    Url(url::ParseError),
    InvalidRepoFormat,
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Reqwest(e) => write!(f, "Reqwest error: {}", e),
            Error::Serde(e) => write!(f, "Serde error: {}", e),
            Error::Io(e) => write!(f, "IO error: {}", e),
            Error::Tokio(e) => write!(f, "Join error: {}", e),
            Error::Url(e) => write!(f, "Error paring URL: {}", e),
            Error::InvalidRepoFormat => write!(f, "Invalid repository format"),
        }
    }
}
impl std::error::Error for Error {}

impl From<reqwest::Error> for Error {
    fn from(e: reqwest::Error) -> Self {
        Error::Reqwest(e)
    }
}
impl From<serde_json::Error> for Error {
    fn from(e: serde_json::Error) -> Self {
        Error::Serde(e)
    }
}
impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::Io(e)
    }
}
impl From<tokio::task::JoinError> for Error {
    fn from(e: tokio::task::JoinError) -> Self {
        Error::Tokio(e)
    }
}
impl From<url::ParseError> for Error {
    fn from(e: url::ParseError) -> Self {
        Error::Url(e)
    }
}

type Result<T> = std::result::Result<T, Error>;

/// A struct that holds the data for a single file.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FileId {
    pub(crate) asset_url: Url,
    pub(crate) chunks: usize,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct AssetsResponse {
    name: String,
    browser_download_url: String,
}

impl FileId {
    /// Uploads a file to the GitHub repository's releases.
    ///
    /// The token must have read and write access to the repository.
    /// `repo` must be in the format `owner/repo`.
    pub async fn upload_file(
        file_name: impl Into<String>,
        file_data: impl Into<Vec<u8>>,
        repo: impl AsRef<str>,
        token: impl AsRef<str>,
    ) -> Result<Self> {
        let repo = repo.as_ref();
        if repo.split('/').count() != 2 && !repo.contains("github.com") {
            return Err(Error::InvalidRepoFormat);
        }

        let mut file_data = file_data.into();
        let file_name = file_name.into();

        let client = client(Some(token));

        let (uploads_url, assets_url) =
            create_or_get_release(repo, "files", client.clone()).await?;

        unsafe {
            prepend_slice(&mut file_data, format!("{file_name}\n").as_bytes());
        }

        let hash = sha256::digest_bytes(&file_data);

        // split the file into chunks of ~100 megabytes
        // I know GitHub allows releases of 2 gigabytes, but it takes too long to upload that much data.
        let chunks = file_data.chunks(100_000_000);
        let chunks_len = chunks.len();

        let assets = client
            .get(assets_url)
            .send()
            .await?
            .json::<Vec<AssetsResponse>>()
            .await?;

        for asset in assets {
            if asset.name == format!("{hash}-chunk0") {
                return Ok(FileId {
                    asset_url: parse_url(&asset.browser_download_url)?,
                    chunks: chunks_len,
                });
            };
        }

        let mut threads = Vec::with_capacity(chunks.len());

        for (i, chunk) in chunks.enumerate() {
            let mut url = uploads_url.clone();
            url.set_query(Some(&format!("name={hash}-chunk{i}")));

            let client = client.clone();
            let chunk = chunk.to_vec();
            threads.push(tokio::spawn(async move {
                client
                    .post(url)
                    .header(reqwest::header::CONTENT_TYPE, "application/octet-stream")
                    .header(reqwest::header::CONTENT_LENGTH, chunk.len())
                    .body(chunk)
                    .send()
                    .await
            }));
        }

        let mut asset_url = MaybeUninit::uninit();
        for (chunk, thread) in threads.into_iter().enumerate() {
            let ret = thread.await??;
            if chunk == 0 {
                let json = ret.json::<AssetsResponse>().await?;
                asset_url.write(parse_url(&json.browser_download_url)?);
            }
        }

        Ok(Self {
            asset_url: unsafe { asset_url.assume_init() },
            chunks: chunks_len,
        })
    }

    /// Downloads the file from the GitHub repository's releases.
    ///
    /// The token must have read access to the repository.
    pub async fn get_file<T: Into<String>>(self, token: Option<T>) -> Result<(Vec<u8>, String)> {
        let mut file = Vec::<u8>::new();
        let mut threads = Vec::with_capacity(self.chunks);

        let client = client(token.map(|t| t.into()));

        for i in 0..self.chunks {
            let client = client.clone();
            let url = format!("{}-chunk{i}", self.asset_url);

            threads.push(tokio::spawn(async move { client.get(url).send().await }));
        }

        for thread in threads {
            let res = thread.await??;
            let chunk = res.bytes().await?;
            file.extend(&chunk);
        }

        let file = file.into_iter();

        let file_name = file
            .clone()
            .map(|b| b as char)
            .take_while(|&c| c != '\n')
            .collect::<String>();

        let file = file.skip(file_name.len() + 1).collect::<Vec<_>>();

        Ok((file, file_name))
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct ReleaseResponse {
    upload_url: Option<String>,
    assets_url: Option<String>,
}

/// Creates a new release on GitHub and returns the `assets_url`.
/// If the release exists, it will only return the `assets_url`.
async fn create_or_get_release(repo: &str, tag: &str, client: Client) -> Result<(Url, Url)> {
    let get_release = || async {
        let url = format!("https://api.github.com/repos/{repo}/releases/tags/{tag}");
        let release = client
            .get(url)
            .send()
            .await?
            .json::<ReleaseResponse>()
            .await?;

        Result::Ok(
            release
                .assets_url
                .and_then(|a| release.upload_url.map(|u| (a, u)))
                .map(|(a, u)| {
                    let url = parse_url(&a).unwrap();
                    let upload_url = parse_url(&u).unwrap();
                    (url, upload_url)
                }),
        )
    };
    let create_release = || async {
        let url = format!("https://api.github.com/repos/{repo}/releases");
        let release = client
            .post(url)
            .json(&serde_json::json!({
                "tag_name": tag,
            }))
            .send()
            .await?
            .json::<ReleaseResponse>()
            .await?;

        Result::Ok(
            release
                .assets_url
                .and_then(|a| release.upload_url.map(|u| (a, u)))
                .map(|(a, u)| {
                    let url = parse_url(&a).unwrap();
                    let upload_url = parse_url(&u).unwrap();
                    (url, upload_url)
                }),
        )
    };

    if let Ok(Some(urls)) = get_release().await {
        Ok(urls)
    } else if let Ok(Some(release)) = create_release().await {
        Ok(release)
    } else {
        // at this point, the repo probably has no commits.
        let url = format!("https://api.github.com/repos/{repo}/contents/__no_empty_repo__",);
        client
            .put(url)
            .json(&serde_json::json!({
                "message": "add a commit to allow creation of a release.",
                "content": "",
                "sha254": "",
            }))
            .send()
            .await?
            .text()
            .await?;

        Ok(get_release().await?.unwrap())
    }
}

unsafe fn prepend_slice<T: Copy>(vec: &mut Vec<T>, slice: &[T]) {
    let len = vec.len();
    let amt = slice.len();
    vec.reserve(amt);

    std::ptr::copy(vec.as_ptr(), vec.as_mut_ptr().add(amt), len);
    std::ptr::copy(slice.as_ptr(), vec.as_mut_ptr(), amt);
    vec.set_len(len + amt);
}

fn client(token: Option<impl AsRef<str>>) -> Client {
    let client = Client::builder().user_agent("Rust").default_headers({
        let mut map = header::HeaderMap::new();
        if let Some(token) = token {
            map.insert(header::AUTHORIZATION, {
                let mut header =
                    header::HeaderValue::from_str(&format!("token {}", token.as_ref())).unwrap();
                header.set_sensitive(true);
                header
            });
        }
        map
    });
    client.build().unwrap()
}

fn parse_url(url: &str) -> Result<Url> {
    let mut url = url.parse::<Url>()?;
    url.set_query(Some(""));

    if let Some(path) = url.clone().path().strip_suffix("%7B") {
        url.set_path(path);
    }

    Ok(url)
}
