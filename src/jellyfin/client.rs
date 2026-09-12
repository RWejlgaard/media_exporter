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
            ClientError::Http(e) => write!(f, "http error: {}", with_causes(e)),
            ClientError::Url(e) => write!(f, "url error: {e}"),
            ClientError::Json(e) => write!(f, "json error: {e}"),
        }
    }
}

impl std::error::Error for ClientError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            ClientError::NotFound => None,
            ClientError::Http(e) => Some(e),
            ClientError::Url(e) => Some(e),
            ClientError::Json(e) => Some(e),
        }
    }
}

/// `reqwest::Error`'s own `Display` stops at "error sending request for url
/// (...)" and says nothing about why the request failed, so a blocked port, a
/// dead host and a DNS failure all log identically. Walking the source chain is
/// what turns that into "tcp connect error: deadline has elapsed".
fn with_causes(e: &reqwest::Error) -> String {
    use std::error::Error;

    let mut out = e.to_string();
    let mut source = e.source();
    while let Some(cause) = source {
        out.push_str(": ");
        out.push_str(&cause.to_string());
        source = cause.source();
    }
    out
}

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

#[cfg(test)]
mod tests {
    use super::*;

    /// Guards the log line an unreachable server produces: without the source
    /// chain it reads only "error sending request for url (...)", which is the
    /// same text a blocked port, a wrong host and a DNS failure all produce.
    #[tokio::test]
    async fn http_error_display_includes_the_underlying_cause() {
        // Port 1 on loopback refuses immediately, so this stays offline and fast.
        let client = Client::new("http://127.0.0.1:1", "token").expect("failed to build client");
        let err = client
            .get::<serde_json::Value>("/")
            .await
            .expect_err("expected a connection failure");

        let message = err.to_string();
        assert!(message.starts_with("http error: "), "unexpected message: {message}");
        assert!(
            message.matches(": ").count() > 1,
            "message carries no cause chain: {message}"
        );
    }
}
