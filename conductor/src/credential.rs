//! Expiry of the Claude credential conductor's sessions run on.
//!
//! A `CLAUDE_CODE_OAUTH_TOKEN` is opaque and cannot renew itself, so whoever
//! creates it writes its expiry date next to it in Vault
//! (`CLAUDE_CODE_OAUTH_TOKEN_EXPIRES_AT`). A credentials file carries its own
//! `refreshTokenExpiresAt`, which moves on every renewal. This module turns
//! whichever one is in use into the gauge
//! `conductor_claude_credential_expiry_timestamp_seconds{source}`.
//!
//! Only the timestamp is ever read out of the file or logged; the token and the
//! rest of the file never leave this module.

use std::path::{Path, PathBuf};
use std::time::Duration;

use chrono::{DateTime, NaiveDate};

/// Name of the expiry gauge.
pub const EXPIRY_GAUGE: &str = "conductor_claude_credential_expiry_timestamp_seconds";

/// Name of the gauge that tells the alert rules whether the expiry applies.
pub const AUTH_MODE_GAUGE: &str = "conductor_claude_auth_mode";

/// How often the credentials file is read again.
pub const REFRESH_INTERVAL: Duration = Duration::from_secs(300);

/// Which credential the expiry belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    /// A static `CLAUDE_CODE_OAUTH_TOKEN`, with its date from the environment.
    OauthToken,
    /// `$HOME/.claude/.credentials.json`.
    CredentialsFile,
}

impl Source {
    /// The value of the `source` label.
    pub fn label(self) -> &'static str {
        match self {
            Source::OauthToken => "oauth_token",
            Source::CredentialsFile => "credentials_file",
        }
    }
}

/// How the pod pays for Claude, as the entrypoint decides it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthMode {
    /// A subscription credential (token or file).
    Subscription,
    /// `ANTHROPIC_API_KEY`: nothing here expires.
    ApiKey,
}

impl AuthMode {
    /// The value of the `mode` label.
    pub fn label(self) -> &'static str {
        match self {
            AuthMode::Subscription => "subscription",
            AuthMode::ApiKey => "api_key",
        }
    }
}

/// The environment variables the expiry depends on.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CredentialEnv {
    /// Whether `CLAUDE_CODE_OAUTH_TOKEN` is set and non-empty.
    pub token_set: bool,
    /// `CLAUDE_CODE_OAUTH_TOKEN_EXPIRES_AT`, if set.
    pub token_expires_at: Option<String>,
    /// `CONDUCTOR_AUTH_MODE`, falling back to the `AUTH_MODE` the entrypoint exports.
    pub auth_mode: Option<String>,
    /// Whether `ANTHROPIC_API_KEY` is set and non-empty.
    pub api_key_set: bool,
}

impl CredentialEnv {
    /// Reads the relevant variables from the process environment.
    pub fn from_process() -> Self {
        let get = |k: &str| std::env::var(k).ok().filter(|v| !v.trim().is_empty());
        Self {
            token_set: get("CLAUDE_CODE_OAUTH_TOKEN").is_some(),
            token_expires_at: get("CLAUDE_CODE_OAUTH_TOKEN_EXPIRES_AT"),
            auth_mode: get("CONDUCTOR_AUTH_MODE").or_else(|| get("AUTH_MODE")),
            api_key_set: get("ANTHROPIC_API_KEY").is_some(),
        }
    }

    /// The billing mode, by the entrypoint's rule: an explicit choice wins,
    /// otherwise a lone API key means `api_key` and anything else is a subscription.
    pub fn mode(&self) -> AuthMode {
        match self.auth_mode.as_deref().map(str::trim) {
            Some("api_key") => AuthMode::ApiKey,
            Some("subscription") => AuthMode::Subscription,
            _ if self.api_key_set && !self.token_set => AuthMode::ApiKey,
            _ => AuthMode::Subscription,
        }
    }
}

/// What reading `$HOME/.claude/.credentials.json` gave.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileState<'a> {
    /// The file does not exist.
    Missing,
    /// The file exists but could not be read.
    Unreadable,
    /// The file's contents.
    Contents(&'a str),
}

/// The outcome of one evaluation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reading {
    /// API-key mode: no expiry applies.
    NotApplicable,
    /// The expiry, in Unix seconds.
    Known(Source, i64),
    /// No usable date, and why. The gauge is left unset.
    Unknown(Source, &'static str),
}

/// Parses an RFC 3339 date-time, or a date alone as 00:00 UTC, to Unix seconds.
pub fn parse_expires_at(raw: &str) -> Option<i64> {
    let raw = raw.trim();
    if let Ok(dt) = DateTime::parse_from_rfc3339(raw) {
        return Some(dt.timestamp());
    }
    let date = NaiveDate::parse_from_str(raw, "%Y-%m-%d").ok()?;
    Some(date.and_hms_opt(0, 0, 0)?.and_utc().timestamp())
}

fn file_expiry(file: FileState<'_>) -> Result<i64, &'static str> {
    let contents = match file {
        FileState::Missing => return Err("credentials file does not exist"),
        FileState::Unreadable => return Err("credentials file is unreadable"),
        FileState::Contents(c) => c,
    };
    let json: serde_json::Value =
        serde_json::from_str(contents).map_err(|_| "credentials file is not valid JSON")?;
    let value = json
        .pointer("/claudeAiOauth/refreshTokenExpiresAt")
        .ok_or("credentials file has no refreshTokenExpiresAt")?;
    let millis = match value {
        serde_json::Value::Null => return Err("refreshTokenExpiresAt is null"),
        v => v
            .as_i64()
            .or_else(|| v.as_u64().and_then(|u| i64::try_from(u).ok()))
            .ok_or("refreshTokenExpiresAt is not an integer")?,
    };
    let secs = millis
        .checked_div(1000)
        .ok_or("refreshTokenExpiresAt is out of range")?;
    if secs <= 0 {
        return Err("refreshTokenExpiresAt is not a plausible date");
    }
    Ok(secs)
}

/// Decides which credential is in use and what its expiry is. Pure: the file
/// is passed in, so no filesystem is needed to test it.
pub fn expiry(env: &CredentialEnv, file: FileState<'_>) -> Reading {
    if env.mode() == AuthMode::ApiKey {
        return Reading::NotApplicable;
    }
    if env.token_set {
        return match env.token_expires_at.as_deref() {
            None => Reading::Unknown(
                Source::OauthToken,
                "CLAUDE_CODE_OAUTH_TOKEN_EXPIRES_AT is not set",
            ),
            Some(raw) => match parse_expires_at(raw) {
                Some(ts) => Reading::Known(Source::OauthToken, ts),
                None => Reading::Unknown(
                    Source::OauthToken,
                    "CLAUDE_CODE_OAUTH_TOKEN_EXPIRES_AT is not an RFC 3339 date or date-time",
                ),
            },
        };
    }
    match file_expiry(file) {
        Ok(ts) => Reading::Known(Source::CredentialsFile, ts),
        Err(why) => Reading::Unknown(Source::CredentialsFile, why),
    }
}

/// The credentials file under `home`.
pub fn credentials_path(home: &Path) -> PathBuf {
    home.join(".claude").join(".credentials.json")
}

/// Applies a reading to the gauges. An unknown reading leaves the expiry gauge
/// untouched: a date is never invented, and a date that was good a moment ago
/// stays until a good one replaces it.
pub fn publish(mode: AuthMode, reading: Reading) {
    for m in [AuthMode::Subscription, AuthMode::ApiKey] {
        metrics::gauge!(AUTH_MODE_GAUGE, "mode" => m.label()).set(if m == mode {
            1.0
        } else {
            0.0
        });
    }
    if let Reading::Known(source, ts) = reading {
        metrics::gauge!(EXPIRY_GAUGE, "source" => source.label()).set(ts as f64);
    }
}

/// Logs a warning when an unknown reading differs from the previous one, and
/// an info line when it recovers. Quiet while the state does not change.
#[derive(Debug, Default)]
pub struct StateLog {
    last: Option<Reading>,
}

impl StateLog {
    /// Records `reading`, logging only on a change of state.
    pub fn observe(&mut self, reading: Reading) {
        let state = |r: &Reading| match r {
            Reading::Unknown(s, why) => Some((*s, *why)),
            _ => None,
        };
        let before = self.last.as_ref().and_then(state);
        let now = state(&reading);
        if before != now {
            match now {
                Some((source, why)) => tracing::warn!(
                    source = source.label(),
                    reason = why,
                    "Claude credential expiry unknown; the expiry gauge is not set"
                ),
                None if self.last.is_some() => {
                    tracing::info!("Claude credential expiry known again")
                }
                None => {}
            }
        }
        self.last = Some(reading);
    }
}

/// Reads the file and the environment once and publishes the result.
pub async fn refresh(home: &Path, log: &mut StateLog) {
    let env = CredentialEnv::from_process();
    let contents = match tokio::fs::read_to_string(credentials_path(home)).await {
        Ok(c) => Some(Ok(c)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(_) => Some(Err(())),
    };
    let file = match &contents {
        None => FileState::Missing,
        Some(Err(())) => FileState::Unreadable,
        Some(Ok(c)) => FileState::Contents(c),
    };
    let reading = expiry(&env, file);
    log.observe(reading);
    publish(env.mode(), reading);
}

/// Publishes the expiry at startup and every [`REFRESH_INTERVAL`], for ever.
pub async fn run(home: PathBuf) {
    let mut log = StateLog::default();
    let mut tick = tokio::time::interval(REFRESH_INTERVAL);
    loop {
        tick.tick().await;
        refresh(&home, &mut log).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn token_env(date: Option<&str>) -> CredentialEnv {
        CredentialEnv {
            token_set: true,
            token_expires_at: date.map(str::to_string),
            ..Default::default()
        }
    }

    const FILE_MS: &str =
        r#"{"claudeAiOauth":{"accessToken":"secret","refreshTokenExpiresAt":1800000000123}}"#;

    #[test]
    fn date_only_is_midnight_utc() {
        assert_eq!(
            expiry(&token_env(Some("2027-11-14")), FileState::Missing),
            Reading::Known(Source::OauthToken, 1_826_150_400)
        );
    }

    #[test]
    fn date_time_is_parsed() {
        assert_eq!(
            expiry(&token_env(Some("2027-11-14T10:00:00Z")), FileState::Missing),
            Reading::Known(Source::OauthToken, 1_826_150_400 + 36_000)
        );
        assert_eq!(
            expiry(
                &token_env(Some("2027-11-14T12:00:00+02:00")),
                FileState::Missing
            ),
            Reading::Known(Source::OauthToken, 1_826_150_400 + 36_000)
        );
    }

    #[test]
    fn invalid_env_value_leaves_gauge_unset() {
        assert!(matches!(
            expiry(&token_env(Some("next year")), FileState::Missing),
            Reading::Unknown(Source::OauthToken, _)
        ));
        assert!(matches!(
            expiry(&token_env(None), FileState::Missing),
            Reading::Unknown(Source::OauthToken, _)
        ));
    }

    #[test]
    fn file_milliseconds_become_seconds() {
        assert_eq!(
            expiry(&CredentialEnv::default(), FileState::Contents(FILE_MS)),
            Reading::Known(Source::CredentialsFile, 1_800_000_000)
        );
    }

    #[test]
    fn file_without_date_or_with_null_is_unknown() {
        for body in [
            r#"{"claudeAiOauth":{"accessToken":"x"}}"#,
            r#"{"claudeAiOauth":{"refreshTokenExpiresAt":null}}"#,
            r#"{"claudeAiOauth":{"refreshTokenExpiresAt":"soon"}}"#,
            r#"{}"#,
        ] {
            assert!(matches!(
                expiry(&CredentialEnv::default(), FileState::Contents(body)),
                Reading::Unknown(Source::CredentialsFile, _)
            ));
        }
    }

    #[test]
    fn torn_json_does_not_panic() {
        assert!(matches!(
            expiry(
                &CredentialEnv::default(),
                FileState::Contents(r#"{"claudeAiOauth":{"refreshTok"#)
            ),
            Reading::Unknown(Source::CredentialsFile, _)
        ));
        assert!(matches!(
            expiry(&CredentialEnv::default(), FileState::Unreadable),
            Reading::Unknown(Source::CredentialsFile, _)
        ));
    }

    #[test]
    fn token_wins_over_file() {
        assert_eq!(
            expiry(&token_env(Some("2027-11-14")), FileState::Contents(FILE_MS)),
            Reading::Known(Source::OauthToken, 1_826_150_400)
        );
    }

    #[test]
    fn api_key_mode_has_no_expiry() {
        let env = CredentialEnv {
            auth_mode: Some("api_key".into()),
            token_set: true,
            token_expires_at: Some("2027-11-14".into()),
            ..Default::default()
        };
        assert_eq!(
            expiry(&env, FileState::Contents(FILE_MS)),
            Reading::NotApplicable
        );
        let inferred = CredentialEnv {
            api_key_set: true,
            ..Default::default()
        };
        assert_eq!(inferred.mode(), AuthMode::ApiKey);
    }

    #[tokio::test]
    async fn value_follows_a_rewritten_file() {
        let home = tempfile::tempdir().expect("tempdir");
        let path = credentials_path(home.path());
        std::fs::create_dir_all(path.parent().expect("has parent")).expect("mkdir");
        let env = CredentialEnv::default();
        let read = |p: &Path| std::fs::read_to_string(p).expect("read");

        std::fs::write(&path, FILE_MS).expect("write");
        assert_eq!(
            expiry(&env, FileState::Contents(&read(&path))),
            Reading::Known(Source::CredentialsFile, 1_800_000_000)
        );
        std::fs::write(
            &path,
            r#"{"claudeAiOauth":{"refreshTokenExpiresAt":1900000000000}}"#,
        )
        .expect("rewrite");
        assert_eq!(
            expiry(&env, FileState::Contents(&read(&path))),
            Reading::Known(Source::CredentialsFile, 1_900_000_000)
        );
    }

    #[test]
    fn state_log_only_changes_on_transition() {
        let mut log = StateLog::default();
        let unknown = Reading::Unknown(Source::OauthToken, "x");
        log.observe(unknown);
        assert_eq!(log.last, Some(unknown));
        log.observe(Reading::Known(Source::OauthToken, 1));
        assert_eq!(log.last, Some(Reading::Known(Source::OauthToken, 1)));
    }
}
