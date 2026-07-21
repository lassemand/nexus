/// Saxo Bank OAuth2 token management.
///
/// Handles token acquisition and in-place WebSocket token refresh.
/// Per ADR-0001 (NEX-77): the access token is renewed via
/// `PUT /ws/authorize?contextid=<id>` without dropping the WebSocket.
///
/// # Rotation semantics
///
/// Saxo rotates the refresh token on every use (RFC 6749 §10.4).
/// `SaxoAuth` keeps the latest refresh token internally and updates it
/// after each successful call, so successive `refresh()` calls all work.
/// The caller receives a `RotatedToken` with both the new access token
/// and the new refresh token — the refresh token must be persisted
/// (written to `saxo_tokens` Postgres table per ADR-0003) by the caller.
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

/// The result of a successful token rotation.
///
/// Both fields must be persisted: the access token is used to reauthorize
/// the WebSocket; the refresh token replaces the previous one for the
/// *next* rotation (the old one is invalidated immediately by Saxo).
#[derive(Debug, Clone)]
pub struct RotatedToken {
    pub access_token: SaxoToken,
    /// The new refresh token to use on the next rotation.
    /// Write this to `saxo_tokens` immediately — the previous value is now invalid.
    pub refresh_token: String,
    /// Expiry of the refresh token itself (~3589 seconds from Saxo).
    pub refresh_token_expires_at: DateTime<Utc>,
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    expires_in: Option<u64>,
    /// Saxo rotates this on every use. If absent the response is invalid.
    refresh_token: Option<String>,
    /// How long the refresh token is valid for (~3589 seconds from Saxo).
    refresh_token_expires_in: Option<u64>,
}

/// Saxo OAuth2 client for token refresh and in-place WebSocket reauthorization.
pub struct SaxoAuth {
    client: reqwest::Client,
    token_url: String,
    client_id: String,
    client_secret: String,
    /// Current refresh token. Updated after each successful rotation so
    /// the next call uses the newly-rotated value.
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

    /// Update the internal refresh token (e.g. after reading a newer value
    /// from the `saxo_tokens` Postgres table on startup).
    pub fn set_refresh_token(&mut self, token: impl Into<String>) {
        self.refresh_token = token.into();
    }

    /// Read the current refresh token (e.g. to initialize a new `SaxoAuth`
    /// instance for a reconnect after a ticker-list change).
    pub fn refresh_token(&self) -> &str {
        &self.refresh_token
    }

    /// Exchange the current refresh token for a new access + refresh token pair.
    ///
    /// Updates the internal refresh token so the next call works correctly.
    /// The caller receives a `RotatedToken` and is responsible for persisting
    /// the new `refresh_token` to Postgres (ADR-0003 write-back).
    pub async fn refresh(&mut self) -> Result<RotatedToken, AuthError> {
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

        let new_refresh = resp
            .refresh_token
            .filter(|t| !t.is_empty())
            .ok_or(AuthError::MissingToken)?;

        self.refresh_token = new_refresh.clone();

        let access_ttl = resp.expires_in.unwrap_or(1200);
        let access_expires_at = Utc::now() + chrono::Duration::seconds(access_ttl as i64);

        let refresh_ttl = resp.refresh_token_expires_in.unwrap_or(3600);
        let refresh_expires_at = Utc::now() + chrono::Duration::seconds(refresh_ttl as i64);

        Ok(RotatedToken {
            access_token: SaxoToken {
                access_token: resp.access_token,
                expires_at: access_expires_at,
            },
            refresh_token: new_refresh,
            refresh_token_expires_at: refresh_expires_at,
        })
    }

    /// Reauthorize an existing WebSocket connection with a new access token.
    /// No reconnect; the connection and all subscriptions stay live.
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rotated_token_fields_are_accessible() {
        let t = RotatedToken {
            access_token: SaxoToken {
                access_token: "access1".to_string(),
                expires_at: Utc::now() + chrono::Duration::seconds(1200),
            },
            refresh_token: "refresh1".to_string(),
            refresh_token_expires_at: Utc::now() + chrono::Duration::seconds(3589),
        };
        assert_eq!(t.refresh_token, "refresh1");
        assert!(!t.access_token.expires_within_secs(-1)); // not yet expired
    }

    #[test]
    fn expires_within_secs_threshold() {
        let soon = SaxoToken {
            access_token: "t".to_string(),
            expires_at: Utc::now() + chrono::Duration::seconds(60),
        };
        assert!(
            soon.expires_within_secs(120),
            "should fire at 2min threshold"
        );
        assert!(
            !soon.expires_within_secs(30),
            "should not fire at 30s threshold"
        );
    }
}
