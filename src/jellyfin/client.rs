use serde::de::DeserializeOwned;
use std::fmt;
use std::time::Duration;
use url::Url;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Debug)]
pub enum ClientError {
    NotFound,
    Http(reqwest::Error),
    Url(url::ParseError),
    Json(serde_json::Error),
}

impl fmt::Display for ClientError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ClientError::NotFound => write!(f, "not found"),
            ClientError::Http(e) => write!(f, "http error: {e}"),
            ClientError::Url(e) => write!(f, "url error: {e}"),
            ClientError::Json(e) => write!(f, "json error: {e}"),
        }
    }
}

impl std::error::Error for ClientError {}

impl From<reqwest::Error> for ClientError {
    fn from(e: reqwest::Error) -> Self {
        ClientError::Http(e)
    }
}

impl From<url::ParseError> for ClientError {
    fn from(e: url::ParseError) -> Self {
        ClientError::Url(e)
    }
}

impl From<serde_json::Error> for ClientError {
    fn from(e: serde_json::Error) -> Self {
        ClientError::Json(e)
    }
}

#[derive(Clone)]
pub struct Client {
    pub token: String,
    pub base_url: Url,
    http: reqwest::Client,
}

impl Client {
    pub fn new(server_url: &str, token: &str) -> Result<Self, ClientError> {
        let base_url = Url::parse(server_url)?;
        let http = reqwest::Client::builder()
            .connect_timeout(CONNECT_TIMEOUT)
            .timeout(REQUEST_TIMEOUT)
            .build()?;
        Ok(Self {
            token: token.to_string(),
            base_url,
            http,
        })
    }

    pub async fn get<T: DeserializeOwned>(&self, path: &str) -> Result<T, ClientError> {
        let url = self.base_url.join(path)?;
        let resp = self
            .http
            .get(url)
            .header("Accept", "application/json")
            .header("Authorization", format!(r#"MediaBrowser Token="{}""#, self.token))
            .send()
            .await?;

        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            return Err(ClientError::NotFound);
        }

        let body = resp.error_for_status()?.bytes().await?;
        Ok(serde_json::from_slice(&body)?)
    }
}
