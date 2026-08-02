//! Saxo Bank OAuth2 authorization-code flow for the `nexus saxo auth` CLI command.
//!
//! Orchestrates the full flow:
//! 1. Generate a CSRF `state` token and build the `/authorize` URL.
//! 2. Print the URL and best-effort open it in the system browser.
//! 3. Bind a one-shot local HTTP listener to catch the authorization-server redirect.
//! 4. Validate the echoed `state`, exchange the `code` for tokens.
//! 5. POST the token pair to the running `saxo_stream` `/tokens` endpoint.
//! 6. Print all token values to stdout so the operator always has a record.

use alpha::saxo::auth::{authorize_url, generate_state, AuthError, SaxoAuth};
use chrono::{DateTime, Utc};
use std::time::Duration;
use thiserror::Error;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;

/// Default local port for the OAuth callback listener.
///
/// Must match the `redirect_uri` registered in the Saxo developer portal
/// (e.g. `http://localhost:7878/callback`).
pub const DEFAULT_CALLBACK_PORT: u16 = 7878;

/// Default timeout in seconds to wait for the browser callback.
///
/// If the user does not complete the authorization flow within this window,
/// [`await_callback`] returns [`CallbackError::Timeout`].
pub const DEFAULT_CALLBACK_TIMEOUT_SECS: u64 = 120;

/// JSON body for `POST /tokens` — sent to the running `saxo_stream` instance
/// to register the initial token pair after the OAuth2 flow completes.
///
/// Matches the `TokenRegistrationBody` contract expected by `chronicle/src/saxo_stream.rs`.
#[derive(serde::Serialize)]
pub struct TokenRegistrationPayload {
    pub access_token: String,
    pub refresh_token: String,
    pub access_token_expires_at: DateTime<Utc>,
    pub refresh_token_expires_at: DateTime<Utc>,
}

/// Run the full Saxo OAuth2 authorization-code flow and register the resulting
/// tokens with a running `saxo_stream` instance.
///
/// # Flow
///
/// 1. Generates a CSRF `state` token via [`generate_state`].
/// 2. Builds the `/authorize` URL and prints it to stderr.
/// 3. Best-effort opens the URL in the system browser (non-fatal if it fails).
/// 4. Waits for the browser redirect on `127.0.0.1:{callback_port}`.
/// 5. Validates the echoed `state`, then exchanges the `code` for tokens via
///    [`SaxoAuth::exchange_code`].
/// 6. Always prints the four token values to **stdout** (even if step 7 fails),
///    so the operator always has a manual fallback.
/// 7. POSTs `{access_token, refresh_token, *_expires_at}` to `register_endpoint`.
///    A non-2xx or network error is printed to stderr but does **not** cause a
///    non-zero exit — the token exchange succeeding is the primary success condition.
///
/// # Errors
///
/// Returns an error (non-zero exit) on: listener timeout, `state` mismatch,
/// OAuth `error` from the authorization server, or token exchange failure.
pub async fn cmd_saxo_auth(
    client_id: &str,
    client_secret: &str,
    auth_base: &str,
    redirect_uri: &str,
    callback_port: u16,
    register_endpoint: &str,
) -> anyhow::Result<()> {
    // 1. Generate CSRF state and build the /authorize URL.
    let state = generate_state();
    let auth_url = authorize_url(auth_base, client_id, redirect_uri, &state)
        .map_err(|e: AuthError| anyhow::anyhow!("failed to build authorize URL: {e}"))?;

    // 2. Print URL and try to open browser (best-effort; non-fatal).
    eprintln!("Open this URL in your browser to authorize:");
    eprintln!("{auth_url}");
    eprintln!();
    if let Err(e) = open::that(&auth_url) {
        eprintln!("(Could not auto-open browser: {e} — please copy the URL above)");
    }

    // 3. Wait for the callback redirect.
    eprintln!("Waiting for callback on port {callback_port}…");
    let callback = await_callback(callback_port, DEFAULT_CALLBACK_TIMEOUT_SECS)
        .await
        .map_err(|e| anyhow::anyhow!("callback listener error: {e}"))?;

    // 4a. Surface any OAuth error from the authorization server (e.g. user denied).
    if let Some(error) = &callback.error {
        let desc = callback
            .error_description
            .as_deref()
            .unwrap_or("(no description)");
        anyhow::bail!("authorization server returned error: {error} — {desc}");
    }

    // 4b. Extract the code and validate the CSRF state.
    let code = callback
        .code
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("callback contained no authorization code"))?;

    let returned_state = callback.state.as_deref().unwrap_or("");
    if returned_state != state {
        anyhow::bail!(
            "CSRF state mismatch — possible replay attack or stale browser tab \
             (expected {state}, got {returned_state})"
        );
    }

    // 5. Exchange the authorization code for tokens.
    let http = reqwest::Client::new();
    let token_url = format!("{auth_base}/token");
    let rotated = SaxoAuth::exchange_code(
        &http,
        &token_url,
        client_id,
        client_secret,
        code,
        redirect_uri,
    )
    .await
    .map_err(|e| anyhow::anyhow!("token exchange failed: {e}"))?;

    // 6. Print token values to stdout — always, even if registration below fails.
    println!("SAXO_REFRESH_TOKEN={}", rotated.refresh_token);
    println!("SAXO_ACCESS_TOKEN={}", rotated.access_token.access_token);
    println!(
        "SAXO_REFRESH_TOKEN_EXPIRES_AT={}",
        rotated.refresh_token_expires_at.to_rfc3339()
    );
    println!(
        "SAXO_ACCESS_TOKEN_EXPIRES_AT={}",
        rotated.access_token.expires_at.to_rfc3339()
    );

    // 7. POST to saxo_stream /tokens endpoint (best-effort).
    let payload = TokenRegistrationPayload {
        access_token: rotated.access_token.access_token.clone(),
        refresh_token: rotated.refresh_token.clone(),
        access_token_expires_at: rotated.access_token.expires_at,
        refresh_token_expires_at: rotated.refresh_token_expires_at,
    };

    match http.post(register_endpoint).json(&payload).send().await {
        Ok(resp) if resp.status().is_success() => {
            eprintln!("✓ tokens registered with saxo_stream at {register_endpoint}");
        }
        Ok(resp) => {
            eprintln!(
                "warning: registration POST returned HTTP {} — \
                 tokens printed above must be delivered manually",
                resp.status()
            );
        }
        Err(e) => {
            eprintln!(
                "warning: registration POST failed ({e}) — \
                 tokens printed above must be delivered manually"
            );
        }
    }

    Ok(())
}

/// Query parameters extracted from the OAuth2 authorization-server redirect.
///
/// All fields are optional at the parse level — semantic validation (e.g.
/// checking that `code` is present, or that `state` matches the expected
/// CSRF token) is the caller's responsibility.
#[derive(Debug, Clone, Default)]
pub struct CallbackParams {
    /// The one-time authorization code to exchange for tokens. Present on success.
    pub code: Option<String>,
    /// The CSRF state token echoed back by the authorization server.
    /// Must be compared against the value generated by
    /// [`alpha::saxo::auth::generate_state`] before proceeding.
    pub state: Option<String>,
    /// OAuth2 error code when the user denies access (e.g. `"access_denied"`).
    pub error: Option<String>,
    /// Human-readable error description accompanying [`error`](Self::error).
    pub error_description: Option<String>,
}

/// Errors returned by [`await_callback`].
#[derive(Debug, Error)]
pub enum CallbackError {
    /// The TCP listener could not be bound to the requested port.
    ///
    /// Most likely cause: the port is already in use by another process.
    /// Check with `lsof -i :{port}` or change the port via the CLI flag.
    #[error("could not bind to port {port}: {source}")]
    Bind {
        port: u16,
        #[source]
        source: std::io::Error,
    },
    /// No browser callback arrived within the configured timeout.
    #[error("timed out after {secs}s waiting for the OAuth callback on port {port}")]
    Timeout { port: u16, secs: u64 },
    /// An I/O error occurred while reading or writing the callback connection.
    #[error("I/O error handling the OAuth callback connection: {0}")]
    Io(std::io::Error),
}

/// Wait for exactly one OAuth2 callback on `http://127.0.0.1:{port}/callback`.
///
/// Binds a `TcpListener` on `127.0.0.1:{port}`, waits up to `timeout_secs`
/// seconds for the browser to deliver the authorization-server redirect, then:
///
/// 1. Parses `code`, `state`, `error`, and `error_description` from the
///    request's query string.
/// 2. Sends a minimal HTML confirmation page so the user sees feedback in
///    the browser before closing the tab.
/// 3. Returns [`CallbackParams`] and drops the listener — the port is free
///    immediately after this call completes.
///
/// # Errors
///
/// - [`CallbackError::Bind`] — port already in use or insufficient permissions.
/// - [`CallbackError::Timeout`] — no request arrived within `timeout_secs`.
/// - [`CallbackError::Io`] — I/O error while reading/writing the connection.
///
/// # Port and redirect URI
///
/// The `port` **must** match the `redirect_uri` you registered in the Saxo
/// developer portal. The default is [`DEFAULT_CALLBACK_PORT`] (7878), giving
/// a redirect URI of `http://localhost:7878/callback`.
pub async fn await_callback(port: u16, timeout_secs: u64) -> Result<CallbackParams, CallbackError> {
    let listener = TcpListener::bind(("127.0.0.1", port))
        .await
        .map_err(|e| CallbackError::Bind { port, source: e })?;

    let (stream, _addr) =
        tokio::time::timeout(Duration::from_secs(timeout_secs), listener.accept())
            .await
            .map_err(|_| CallbackError::Timeout {
                port,
                secs: timeout_secs,
            })?
            .map_err(CallbackError::Io)?;

    let (read_half, mut write_half) = stream.into_split();
    let mut reader = BufReader::new(read_half);

    // First line of the HTTP request: `GET /callback?code=...&state=... HTTP/1.1`
    let mut request_line = String::new();
    reader
        .read_line(&mut request_line)
        .await
        .map_err(CallbackError::Io)?;

    let params = parse_query_from_request_line(&request_line);

    // Drain remaining request headers so the browser does not see a connection reset.
    loop {
        let mut line = String::new();
        reader
            .read_line(&mut line)
            .await
            .map_err(CallbackError::Io)?;
        if line.trim().is_empty() {
            break;
        }
    }

    // Deliver a confirmation page — the user is looking at a browser tab.
    const BODY: &str = "<!DOCTYPE html>\
        <html lang=\"en\">\
        <head><meta charset=\"utf-8\"><title>Authorization received</title></head>\
        <body>\
        <h1>Authorization received</h1>\
        <p>You can close this tab and return to the terminal.</p>\
        </body></html>";

    let response = format!(
        "HTTP/1.1 200 OK\r\n\
         Content-Type: text/html; charset=utf-8\r\n\
         Content-Length: {len}\r\n\
         Connection: close\r\n\
         \r\n\
         {BODY}",
        len = BODY.len(),
    );
    write_half
        .write_all(response.as_bytes())
        .await
        .map_err(CallbackError::Io)?;
    write_half.flush().await.map_err(CallbackError::Io)?;

    Ok(params)
}

/// Parse `code`, `state`, `error`, and `error_description` from an HTTP/1.x
/// request line such as `GET /callback?code=ABC&state=XYZ HTTP/1.1`.
///
/// Returns a [`CallbackParams`] with all absent fields as `None`.
fn parse_query_from_request_line(request_line: &str) -> CallbackParams {
    let query = request_line
        .split_whitespace()
        .nth(1)
        .and_then(|path_query| path_query.split_once('?'))
        .map(|(_, q)| q)
        .unwrap_or("");

    let mut params = CallbackParams::default();
    for pair in query.split('&') {
        let Some((key, raw_value)) = pair.split_once('=') else {
            continue;
        };
        let value = percent_decode(raw_value);
        match key {
            "code" => params.code = Some(value),
            "state" => params.state = Some(value),
            "error" => params.error = Some(value),
            "error_description" => params.error_description = Some(value),
            _ => {}
        }
    }
    params
}

/// Minimal percent-decoding for OAuth2 query values.
///
/// Decodes `%XX` sequences (hex byte) and `+` (space). Sufficient for the
/// ASCII-safe parameter values returned by Saxo's authorization server.
fn percent_decode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(byte) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                out.push(byte as char);
                i += 3;
                continue;
            }
        }
        out.push(if bytes[i] == b'+' {
            ' '
        } else {
            bytes[i] as char
        });
        i += 1;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── parse_query_from_request_line ─────────────────────────────────────

    #[test]
    fn parse_success_params() {
        let line = "GET /callback?code=AUTH123&state=deadbeef HTTP/1.1\r\n";
        let p = parse_query_from_request_line(line);
        assert_eq!(p.code.as_deref(), Some("AUTH123"));
        assert_eq!(p.state.as_deref(), Some("deadbeef"));
        assert!(p.error.is_none());
        assert!(p.error_description.is_none());
    }

    #[test]
    fn parse_error_denial_params() {
        let line =
            "GET /callback?error=access_denied&error_description=User+denied+access HTTP/1.1\r\n";
        let p = parse_query_from_request_line(line);
        assert!(p.code.is_none());
        assert_eq!(p.error.as_deref(), Some("access_denied"));
        assert_eq!(p.error_description.as_deref(), Some("User denied access"));
    }

    #[test]
    fn parse_no_query_string_returns_all_none() {
        let p = parse_query_from_request_line("GET /callback HTTP/1.1\r\n");
        assert!(p.code.is_none());
        assert!(p.state.is_none());
        assert!(p.error.is_none());
    }

    #[test]
    fn parse_empty_request_line_returns_all_none() {
        let p = parse_query_from_request_line("");
        assert!(p.code.is_none());
    }

    #[test]
    fn parse_unknown_params_are_ignored() {
        let line = "GET /callback?code=X&unknown=foo&state=Y HTTP/1.1\r\n";
        let p = parse_query_from_request_line(line);
        assert_eq!(p.code.as_deref(), Some("X"));
        assert_eq!(p.state.as_deref(), Some("Y"));
    }

    // ── percent_decode ────────────────────────────────────────────────────

    #[test]
    fn percent_decode_hex_sequences() {
        assert_eq!(percent_decode("hello%20world"), "hello world");
        assert_eq!(percent_decode("foo%2Bbar"), "foo+bar");
        assert_eq!(percent_decode("a%3Db"), "a=b");
    }

    #[test]
    fn percent_decode_plus_as_space() {
        assert_eq!(percent_decode("hello+world"), "hello world");
    }

    #[test]
    fn percent_decode_passthrough() {
        assert_eq!(percent_decode("plain"), "plain");
        assert_eq!(percent_decode(""), "");
    }

    #[test]
    fn percent_decode_invalid_percent_passes_through() {
        assert_eq!(percent_decode("%GG"), "%GG");
    }

    // ── await_callback integration tests (loopback) ───────────────────────

    #[tokio::test]
    async fn await_callback_returns_params_on_connection() {
        use tokio::io::AsyncWriteExt;
        use tokio::net::TcpStream;

        let port = 19871u16;

        let listener_task = tokio::spawn(await_callback(port, 5));
        // Give the listener a moment to bind before connecting.
        tokio::time::sleep(Duration::from_millis(20)).await;

        let mut conn = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        conn.write_all(
            b"GET /callback?code=TESTCODE&state=TESTSTATE HTTP/1.1\r\n\
              Host: localhost\r\n\
              \r\n",
        )
        .await
        .unwrap();

        let result = listener_task
            .await
            .unwrap()
            .expect("listener should succeed");
        assert_eq!(result.code.as_deref(), Some("TESTCODE"));
        assert_eq!(result.state.as_deref(), Some("TESTSTATE"));
        assert!(result.error.is_none());
    }

    #[tokio::test]
    async fn await_callback_times_out() {
        // Use a 1-second timeout so the test suite doesn't stall.
        let result = await_callback(19872, 1).await;
        assert!(
            matches!(result, Err(CallbackError::Timeout { .. })),
            "expected Timeout, got: {result:?}"
        );
    }

    #[tokio::test]
    async fn await_callback_bind_fails_on_used_port() {
        let port = 19873u16;
        // Hold the port so await_callback cannot bind it.
        let _occupied = TcpListener::bind(("127.0.0.1", port)).await.unwrap();

        let result = await_callback(port, 5).await;
        assert!(
            matches!(result, Err(CallbackError::Bind { .. })),
            "expected Bind error, got: {result:?}"
        );
    }
}
