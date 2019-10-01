//! Twitch API helpers.

use bytes::Bytes;
use reqwest::{header, r#async::Client, Method, Url};

const ID_TWITCH_URL: &'static str = "https://id.twitch.tv";

/// API integration.
#[derive(Clone, Debug)]
pub struct IdTwitchClient {
    client: Client,
    api_url: Url,
}

impl IdTwitchClient {
    /// Create a new API integration.
    pub fn new() -> Result<IdTwitchClient, failure::Error> {
        Ok(IdTwitchClient {
            client: Client::new(),
            api_url: str::parse::<Url>(ID_TWITCH_URL)?,
        })
    }

    /// Get request against API.
    fn request(&self, method: Method, path: &[&str]) -> RequestBuilder {
        let mut url = self.api_url.clone();
        url.path_segments_mut().expect("bad base").extend(path);

        RequestBuilder {
            client: self.client.clone(),
            url,
            method,
            headers: Vec::new(),
            body: None,
        }
    }

    // Validate the specified token through twitch validation API.
    pub async fn validate_token(&self, token: &str) -> Result<ValidateToken, failure::Error> {
        let request = self
            .request(Method::GET, &["oauth2", "validate"])
            .header(header::AUTHORIZATION, &format!("OAuth {}", token));

        request.execute().await
    }
}

/// Response from the validate token endpoint.
#[derive(Debug, serde::Deserialize)]
pub struct ValidateToken {
    pub client_id: String,
    pub login: String,
    pub scopes: Vec<String>,
    pub user_id: String,
}

struct RequestBuilder {
    client: Client,
    url: Url,
    method: Method,
    headers: Vec<(header::HeaderName, String)>,
    body: Option<Bytes>,
}

impl RequestBuilder {
    /// Execute the request.
    pub async fn execute<T>(self) -> Result<T, failure::Error>
    where
        T: serde::de::DeserializeOwned,
    {
        let mut r = self.client.request(self.method, self.url);

        if let Some(body) = self.body {
            r = r.body(body);
        }

        for (key, value) in self.headers {
            r = r.header(key, value);
        }

        let res = r.send().await?;
        let status = res.status();
        let body = res.bytes().await?;

        if !status.is_success() {
            failure::bail!(
                "bad response: {}: {}",
                status,
                String::from_utf8_lossy(body.as_ref())
            );
        }

        log::trace!("response: {}", String::from_utf8_lossy(body.as_ref()));
        serde_json::from_slice(body.as_ref()).map_err(Into::into)
    }

    /// Push a header.
    pub fn header(mut self, key: header::HeaderName, value: &str) -> Self {
        self.headers.push((key, value.to_string()));
        self
    }
}
