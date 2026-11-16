//! The reviewer's own Linear identity: an OAuth app acting as itself.
//!
//! Linear's client-credentials grant issues a token with the `app` actor, so
//! every comment and state change made with it is shown under the app's name
//! and icon, and it uses no paid seat. The token lasts 30 days and comes with
//! no refresh token; a new one is simply requested from the client id and
//! secret. Nothing is stored: the token lives in memory and is fetched again
//! after a restart.
//!
//! The client credentials grant has to be switched on for the app in Linear
//! (Settings → API → OAuth applications), otherwise Linear answers
//! `Client does not support the client_credentials grant type`.

use std::time::{Duration, Instant};

use serde::Deserialize;
use tokio::sync::Mutex;

use crate::linear_api::LinearApiError;

/// Linear's token endpoint; `CONDUCTOR_LINEAR_OAUTH_URL` overrides it.
pub const DEFAULT_TOKEN_URL: &str = "https://api.linear.app/oauth/token";

/// Scopes the reviewer needs: read the issue, move it, comment on it.
pub const SCOPES: &str = "read,write,comments:create";

/// A token is renewed this long before it expires, so a write never races the
/// expiry.
const RENEW_BEFORE: Duration = Duration::from_secs(24 * 3600);

const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    expires_in: Option<u64>,
}

/// Fetches and caches the app's client-credentials token.
pub struct AppTokenSource {
    http: reqwest::Client,
    token_url: String,
    client_id: String,
    client_secret: String,
    cached: Mutex<Option<(String, Instant)>>,
}

impl std::fmt::Debug for AppTokenSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AppTokenSource")
            .field("token_url", &self.token_url)
            .field("client_id", &self.client_id)
            .field("client_secret", &"<redacted>")
            .finish()
    }
}

impl AppTokenSource {
    /// Builds a source. `token_url` overrides Linear's endpoint, for tests.
    pub fn new(
        client_id: impl Into<String>,
        client_secret: impl Into<String>,
        token_url: Option<String>,
    ) -> Result<Self, LinearApiError> {
        let http = reqwest::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .build()
            .map_err(|e| LinearApiError::Network(e.to_string()))?;
        Ok(Self {
            http,
            token_url: token_url.unwrap_or_else(|| DEFAULT_TOKEN_URL.to_string()),
            client_id: client_id.into(),
            client_secret: client_secret.into(),
            cached: Mutex::new(None),
        })
    }

    /// A valid access token, fetching a new one when none is cached or the
    /// cached one is close to expiry.
    pub async fn token(&self) -> Result<String, LinearApiError> {
        let mut cached = self.cached.lock().await;
        if let Some((token, expires_at)) = cached.as_ref() {
            if Instant::now() + RENEW_BEFORE < *expires_at {
                return Ok(token.clone());
            }
        }
        let fetched = self.fetch().await;
        match fetched {
            Ok((token, lifetime)) => {
                *cached = Some((token.clone(), Instant::now() + lifetime));
                Ok(token)
            }
            Err(e) => {
                metrics::counter!("conductor_review_linear_auth_failures_total").increment(1);
                Err(e)
            }
        }
    }

    /// Drops the cached token, so the next [`AppTokenSource::token`] fetches a
    /// new one. Called after Linear rejects the token.
    pub async fn invalidate(&self) {
        *self.cached.lock().await = None;
    }

    async fn fetch(&self) -> Result<(String, Duration), LinearApiError> {
        let response = self
            .http
            .post(&self.token_url)
            .form(&[
                ("grant_type", "client_credentials"),
                ("scope", SCOPES),
                ("client_id", self.client_id.as_str()),
                ("client_secret", self.client_secret.as_str()),
            ])
            .send()
            .await
            .map_err(|e| LinearApiError::Network(e.without_url().to_string()))?;
        let status = response.status().as_u16();
        if status != 200 {
            return Err(LinearApiError::Status(status));
        }
        let body: TokenResponse = response
            .json()
            .await
            .map_err(|e| LinearApiError::Malformed(e.without_url().to_string()))?;
        let lifetime = Duration::from_secs(body.expires_in.unwrap_or(30 * 24 * 3600));
        tracing::info!(
            expires_in_secs = lifetime.as_secs(),
            "fetched the reviewer's Linear app token"
        );
        Ok((body.access_token, lifetime))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{body_string_contains, method};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn token(access: &str, expires_in: u64) -> ResponseTemplate {
        ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "access_token": access, "token_type": "Bearer",
            "expires_in": expires_in, "scope": "read write"
        }))
    }

    #[tokio::test]
    async fn client_credentials_are_posted_and_the_token_cached() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(body_string_contains("grant_type=client_credentials"))
            .and(body_string_contains("client_id=cid"))
            .and(body_string_contains(
                "scope=read%2Cwrite%2Ccomments%3Acreate",
            ))
            .respond_with(token("app-1", 2_591_999))
            .expect(1)
            .mount(&server)
            .await;
        let source = AppTokenSource::new("cid", "secret", Some(server.uri())).unwrap();
        assert_eq!(source.token().await.unwrap(), "app-1");
        assert_eq!(source.token().await.unwrap(), "app-1", "served from cache");
    }

    #[tokio::test]
    async fn a_token_close_to_expiry_is_renewed() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(token("short", 60))
            .expect(2)
            .mount(&server)
            .await;
        let source = AppTokenSource::new("cid", "secret", Some(server.uri())).unwrap();
        source.token().await.unwrap();
        source.token().await.unwrap();
    }

    #[tokio::test]
    async fn a_refused_grant_is_an_error() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(400))
            .mount(&server)
            .await;
        let source = AppTokenSource::new("cid", "secret", Some(server.uri())).unwrap();
        assert!(matches!(
            source.token().await,
            Err(LinearApiError::Status(400))
        ));
    }

    #[test]
    fn debug_redacts_the_secret() {
        let source = AppTokenSource::new("cid", "hunter2", None).unwrap();
        assert!(!format!("{source:?}").contains("hunter2"));
    }
}
