/// Saxo Bank OAuth2 token management.
///
/// Handles token acquisition and in-place WebSocket token refresh.
/// Per NEX-77 / ADR-0001: tokens are renewed via
/// `PUT /ws/authorize?contextid=<id>` — the WebSocket connection is
/// NOT dropped on refresh.
use chrono::{DateTime, Utc};
use serde::Deserialize;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum AuthError {
    #[error("HTTP error: {0}")]
    Http(#[from] reqwest::Error),
    #[error("token response missing access_token")]
    MissingToken,
}

/// A Saxo Bank OAuth2 access token with its expiry time.
#[derive(Debug, Clone)]
pub struct SaxoToken {
    pub access_token: String,
    pub expires_at: DateTime<Utc>,
}

impl SaxoToken {
    /// Returns true if the token expires within the given number of seconds.
    pub fn expires_within_secs(&self, secs: i64) -> bool {
        let threshold = Utc::now() + chrono::Duration::seconds(secs);
        self.expires_at <= threshold
    }
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    expires_in: Option<u64>,
}

/// Saxo OAuth2 client credentials for token acquisition and refresh.
pub struct SaxoAuth {
    client: reqwest::Client,
    token_url: String,
    client_id: String,
    client_secret: String,
    refresh_token: String,
}

impl SaxoAuth {
    pub fn new(
        client: reqwest::Client,
        token_url: impl Into<String>,
        client_id: impl Into<String>,
        client_secret: impl Into<String>,
        refresh_token: impl Into<String>,
    ) -> Self {
        Self {
            client,
            token_url: token_url.into(),
            client_id: client_id.into(),
            client_secret: client_secret.into(),
            refresh_token: refresh_token.into(),
        }
    }

    /// Exchange the current refresh token for a new access token.
    ///
    /// Saxo rotates refresh tokens on each use — the caller must persist
    /// the new refresh token returned here.
    pub async fn refresh(&mut self) -> Result<SaxoToken, AuthError> {
        let resp: TokenResponse = self
            .client
            .post(&self.token_url)
            .form(&[
                ("grant_type", "refresh_token"),
                ("refresh_token", &self.refresh_token),
                ("client_id", &self.client_id),
                ("client_secret", &self.client_secret),
            ])
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;

        if resp.access_token.is_empty() {
            return Err(AuthError::MissingToken);
        }

        let ttl = resp.expires_in.unwrap_or(1200);
        let expires_at = Utc::now() + chrono::Duration::seconds(ttl as i64);

        Ok(SaxoToken {
            access_token: resp.access_token,
            expires_at,
        })
    }

    /// Refresh the token on an existing WebSocket connection without
    /// disconnecting. Uses the Saxo-specific authorize endpoint:
    /// `PUT <streaming_base>/authorize?contextid=<id>`
    pub async fn refresh_on_stream(
        &self,
        streaming_base: &str,
        context_id: &str,
        new_token: &str,
    ) -> Result<(), AuthError> {
        let url = format!("{streaming_base}/authorize?contextid={context_id}");
        self.client
            .put(&url)
            .bearer_auth(new_token)
            .send()
            .await?
            .error_for_status()?;
        Ok(())
    }
}
