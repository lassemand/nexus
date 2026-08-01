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
/// (written to `oauth_tokens` Postgres table per ADR-0003) by the caller.
use chrono::{DateTime, Utc};
use serde::Deserialize;
use std::sync::{Arc, Mutex};
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

/// The current access token, shared between the periodic refresh task (writer)
/// and any reader that needs the latest value (e.g. `SaxoBarStream`'s connect/
/// reconnect logic). A `std::sync::Mutex` is sufficient since critical sections
/// are a plain struct read/write, never held across an `.await`.
pub type SharedToken = Arc<Mutex<SaxoToken>>;

/// The result of a successful token rotation.
///
/// Both fields must be persisted: the access token is used to reauthorize
/// the WebSocket; the refresh token replaces the previous one for the
/// *next* rotation (the old one is invalidated immediately by Saxo).
#[derive(Debug, Clone)]
pub struct RotatedToken {
    pub access_token: SaxoToken,
    /// The new refresh token to use on the next rotation.
    /// Write this to `oauth_tokens` immediately — the previous value is now invalid.
    pub refresh_token: String,
    /// Expiry of the refresh token itself (~3589 seconds from Saxo).
    pub refresh_token_expires_at: DateTime<Utc>,
}

/// Durably persists a rotated refresh token.
///
/// `alpha` has no Postgres dependency, so this trait is how `SaxoAuth` owns
/// persistence as an integral part of `refresh()` (per ADR-0003) without the
/// caller having to remember to do it separately. The concrete implementation
/// (backed by the `oauth_tokens` table) lives in `chronicle`, which owns the
/// DB pool, and is handed to `SaxoAuth::new` as a plain trait object.
///
/// Infallible by design: implementations are responsible for logging their
/// own failures. A transient persistence failure must not unwind a
/// successful rotation — the new access token is already valid and must
/// still be used to reauthorize the WebSocket regardless.
#[async_trait::async_trait]
pub trait TokenStore: Send + Sync {
    async fn save(&self, rotated: &RotatedToken);
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
    store: Arc<dyn TokenStore>,
}

impl SaxoAuth {
    pub fn new(
        client: reqwest::Client,
        token_url: impl Into<String>,
        client_id: impl Into<String>,
        client_secret: impl Into<String>,
        refresh_token: impl Into<String>,
        store: Arc<dyn TokenStore>,
    ) -> Self {
        Self {
            client,
            token_url: token_url.into(),
            client_id: client_id.into(),
            client_secret: client_secret.into(),
            refresh_token: refresh_token.into(),
            store,
        }
    }

    /// Exchange the current refresh token for a new access + refresh token pair.
    ///
    /// Updates the internal refresh token so the next call works correctly,
    /// and persists the rotation via the configured `TokenStore` before
    /// returning — the caller never needs to persist this itself.
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

        let rotated = RotatedToken {
            access_token: SaxoToken {
                access_token: resp.access_token,
                expires_at: access_expires_at,
            },
            refresh_token: new_refresh,
            refresh_token_expires_at: refresh_expires_at,
        };

        self.store.save(&rotated).await;

        Ok(rotated)
    }

    /// Exchange a one-time authorization code for the initial access + refresh token pair.
    ///
    /// This is the first leg of the OAuth2 Authorization Code Grant — called once after
    /// the user completes the browser redirect and the authorization server returns a `code`.
    /// Unlike [`refresh`](Self::refresh), this is a free function: there is no prior token
    /// state and no persistence — the caller receives the [`RotatedToken`] and decides what
    /// to do with it.
    ///
    /// # Errors
    ///
    /// - [`AuthError::Http`] — non-2xx response from the token endpoint.
    /// - [`AuthError::MissingToken`] — the response JSON lacks a non-empty `access_token`
    ///   or `refresh_token`.
    pub async fn exchange_code(
        client: &reqwest::Client,
        token_url: &str,
        client_id: &str,
        client_secret: &str,
        code: &str,
        redirect_uri: &str,
    ) -> Result<RotatedToken, AuthError> {
        let resp: TokenResponse = client
            .post(token_url)
            .form(&[
                ("grant_type", "authorization_code"),
                ("code", code),
                ("redirect_uri", redirect_uri),
                ("client_id", client_id),
                ("client_secret", client_secret),
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

    /// Validates that `exchange_code` correctly maps the TTL fields.
    ///
    /// Uses a mock HTTP server so no real Saxo endpoint is required.
    #[tokio::test]
    async fn exchange_code_parses_ttls_correctly() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/token"))
            .respond_with(
                ResponseTemplate::new(200)
                    .append_header("content-type", "application/json")
                    .set_body_string(
                        r#"{
                            "access_token": "acc123",
                            "expires_in": 900,
                            "refresh_token": "ref456",
                            "refresh_token_expires_in": 7200
                        }"#,
                    ),
            )
            .mount(&server)
            .await;

        let client = reqwest::Client::new();
        let token_url = format!("{}/token", server.uri());
        let result = SaxoAuth::exchange_code(
            &client,
            &token_url,
            "client_id",
            "client_secret",
            "auth_code",
            "https://localhost/callback",
        )
        .await
        .expect("exchange_code should succeed");

        assert_eq!(result.access_token.access_token, "acc123");
        assert_eq!(result.refresh_token, "ref456");

        // Access token should expire in ~900s; verify it's in the future and
        // less than 1200s from now (the default).
        let access_secs_remaining = (result.access_token.expires_at - Utc::now()).num_seconds();
        assert!(
            access_secs_remaining > 0 && access_secs_remaining <= 900,
            "access TTL out of range: {access_secs_remaining}"
        );

        // Refresh token should expire in ~7200s.
        let refresh_secs_remaining = (result.refresh_token_expires_at - Utc::now()).num_seconds();
        assert!(
            refresh_secs_remaining > 0 && refresh_secs_remaining <= 7200,
            "refresh TTL out of range: {refresh_secs_remaining}"
        );
    }

    #[tokio::test]
    async fn exchange_code_returns_missing_token_on_empty_access_token() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/token"))
            .respond_with(
                ResponseTemplate::new(200)
                    .append_header("content-type", "application/json")
                    .set_body_string(r#"{"access_token": "", "refresh_token": "ref456"}"#),
            )
            .mount(&server)
            .await;

        let client = reqwest::Client::new();
        let token_url = format!("{}/token", server.uri());
        let err = SaxoAuth::exchange_code(
            &client,
            &token_url,
            "id",
            "secret",
            "code",
            "https://localhost/callback",
        )
        .await
        .expect_err("should fail on empty access_token");

        assert!(
            matches!(err, AuthError::MissingToken),
            "expected MissingToken, got: {err}"
        );
    }

    #[tokio::test]
    async fn exchange_code_returns_missing_token_on_absent_refresh_token() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/token"))
            .respond_with(
                ResponseTemplate::new(200)
                    .append_header("content-type", "application/json")
                    .set_body_string(r#"{"access_token": "acc123"}"#),
            )
            .mount(&server)
            .await;

        let client = reqwest::Client::new();
        let token_url = format!("{}/token", server.uri());
        let err = SaxoAuth::exchange_code(
            &client,
            &token_url,
            "id",
            "secret",
            "code",
            "https://localhost/callback",
        )
        .await
        .expect_err("should fail on absent refresh_token");

        assert!(
            matches!(err, AuthError::MissingToken),
            "expected MissingToken, got: {err}"
        );
    }

    #[tokio::test]
    async fn exchange_code_surfaces_http_error_on_non_2xx() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/token"))
            .respond_with(
                ResponseTemplate::new(400).set_body_string(r#"{"error":"invalid_grant"}"#),
            )
            .mount(&server)
            .await;

        let client = reqwest::Client::new();
        let token_url = format!("{}/token", server.uri());
        let err = SaxoAuth::exchange_code(
            &client,
            &token_url,
            "id",
            "secret",
            "bad_code",
            "https://localhost/callback",
        )
        .await
        .expect_err("should fail on 400");

        assert!(
            matches!(err, AuthError::Http(_)),
            "expected Http error, got: {err}"
        );
    }
}
