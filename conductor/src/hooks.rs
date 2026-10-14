//! Claude Code hook intake.
//!
//! Interactive sessions never exit, so busy/idle is learned from hooks instead:
//! Claude runs `conductor hook` for `SessionStart`, `UserPromptSubmit`, `Stop`
//! and `SessionEnd`, which forwards the hook JSON to a loopback-only listener
//! inside `conductor serve`. Hooks fire for prompts a human typed as well as for
//! pasted ones.
//!
//! # Does `Stop` fire when a turn is interrupted with Escape?
//!
//! **Not verified.** The sandbox this was written in has neither tmux nor
//! Claude credentials, so it could not be run against the pinned version in
//! `infra/Dockerfile.conductor`. Claude Code's hook documentation says `Stop`
//! runs "when the main agent has finished responding" and notes it does *not*
//! run on user interrupts, which suggests it does not fire. Until someone
//! confirms with a real run, treat an interrupted turn as possibly leaving the
//! group `Busy` with no `Stop` coming: timeout handling must not rely on `Stop`
//! after `interrupt`, and should poll the pane or force a fresh state itself.

use std::sync::Arc;
use std::time::Duration;

use axum::{extract::State, http::StatusCode, routing::post, Router};
use serde::Deserialize;
use uuid::Uuid;

use crate::registry::{HookEvent, Registry};
use crate::tmux::SessionRuntime;

/// Default port of the loopback hook listener.
pub const DEFAULT_HOOK_PORT: u16 = 8789;
/// Environment variable overriding the hook port.
pub const HOOK_PORT_ENV: &str = "CONDUCTOR_HOOK_PORT";
/// How long `conductor hook` waits for the server.
pub const HOOK_TIMEOUT: Duration = Duration::from_secs(2);

/// The hook port from the environment.
pub fn hook_port() -> u16 {
    std::env::var(HOOK_PORT_ENV)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(DEFAULT_HOOK_PORT)
}

/// The fields of Claude's hook payload that conductor uses.
///
/// Lenient on purpose: payloads gain fields between Claude Code versions, and
/// unknown ones are ignored.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct HookPayload {
    pub hook_event_name: String,
    pub session_id: String,
    pub cwd: String,
    pub transcript_path: String,
}

impl HookPayload {
    /// The state-machine event for this hook, if conductor cares about it.
    pub fn event(&self) -> Option<HookEvent> {
        match self.hook_event_name.as_str() {
            "SessionStart" => Some(HookEvent::SessionStart),
            "UserPromptSubmit" => Some(HookEvent::UserPromptSubmit),
            "Stop" => Some(HookEvent::Stop),
            "SessionEnd" => Some(HookEvent::SessionEnd),
            _ => None,
        }
    }
}

/// State for the hook listener.
#[derive(Clone)]
pub struct HookState {
    pub registry: Registry,
    pub runtime: Arc<dyn SessionRuntime>,
}

/// Router for the loopback listener. Kept apart from the public webhook router.
pub fn router(state: HookState) -> Router {
    Router::new()
        .route("/hook", post(handle_hook))
        .with_state(state)
}

/// Binds `addr` (which must be loopback) and serves hooks until stopped.
pub async fn serve(addr: &str, state: HookState) -> std::io::Result<()> {
    let listener = tokio::net::TcpListener::bind(addr).await?;
    tracing::info!(addr, "hook listener up");
    axum::serve(listener, router(state)).await
}

/// `POST /hook`.
///
/// `204` for everything accepted or deliberately ignored; unknown sessions are
/// the entrypoint smoke check and ad-hoc Claude runs, not errors.
async fn handle_hook(State(state): State<HookState>, body: axum::body::Bytes) -> StatusCode {
    let payload: HookPayload = match serde_json::from_slice(&body) {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!(error = %e, "unparseable hook payload");
            return StatusCode::BAD_REQUEST;
        }
    };
    let (Some(event), Ok(session_id)) = (payload.event(), Uuid::parse_str(&payload.session_id))
    else {
        tracing::debug!(event = %payload.hook_event_name, "hook ignored");
        return StatusCode::NO_CONTENT;
    };

    let slug = match state.registry.slug_for_session(session_id).await {
        Ok(Some(slug)) => slug,
        Ok(None) => {
            tracing::debug!(%session_id, event = %payload.hook_event_name, "hook for unknown session");
            return StatusCode::NO_CONTENT;
        }
        Err(e) => {
            tracing::error!(error = %e, "hook lookup failed");
            return StatusCode::INTERNAL_SERVER_ERROR;
        }
    };

    // Read before the transaction so a slow tmux never holds the group lock.
    let pane_pid = match event {
        HookEvent::SessionStart | HookEvent::Stop => state.runtime.pane_pid(&slug).await,
        _ => None,
    }
    .unwrap_or(0);

    match state.registry.apply_hook(session_id, event, pane_pid).await {
        Ok(Some((slug, from, to))) => {
            tracing::info!(group = %slug, hook = %payload.hook_event_name, ?from, ?to, "hook applied");
            StatusCode::NO_CONTENT
        }
        Ok(None) => {
            tracing::debug!(group = %slug, hook = %payload.hook_event_name, "out-of-order hook ignored");
            StatusCode::NO_CONTENT
        }
        Err(e) => {
            tracing::error!(error = %e, "applying hook failed");
            StatusCode::INTERNAL_SERVER_ERROR
        }
    }
}

/// Forwards one hook payload to the listener at `url`.
///
/// Failures are returned for the caller to log on stderr; `conductor hook`
/// still exits 0, because a non-zero exit (a `Stop` hook exiting 2 blocks
/// stopping) or any stdout output can change Claude's behaviour.
pub async fn forward(url: &str, body: Vec<u8>) -> Result<(), String> {
    let client = reqwest::Client::builder()
        .timeout(HOOK_TIMEOUT)
        .build()
        .map_err(|e| e.to_string())?;
    let resp = client
        .post(url)
        .header("content-type", "application/json")
        .body(body)
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if resp.status().is_success() {
        Ok(())
    } else {
        Err(format!("conductor answered {}", resp.status()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn payload_tolerates_missing_and_unknown_fields() {
        let p: HookPayload = serde_json::from_str(
            r#"{"hook_event_name":"Stop","session_id":"x","brand_new_field":{"a":1}}"#,
        )
        .unwrap();
        assert_eq!(p.event(), Some(HookEvent::Stop));
        assert_eq!(p.cwd, "");
        let empty: HookPayload = serde_json::from_str("{}").unwrap();
        assert_eq!(empty.event(), None);
    }

    #[test]
    fn only_the_four_lifecycle_hooks_map() {
        for (name, ev) in [
            ("SessionStart", Some(HookEvent::SessionStart)),
            ("UserPromptSubmit", Some(HookEvent::UserPromptSubmit)),
            ("Stop", Some(HookEvent::Stop)),
            ("SessionEnd", Some(HookEvent::SessionEnd)),
            ("Notification", None),
            ("PreToolUse", None),
        ] {
            let p = HookPayload {
                hook_event_name: name.into(),
                ..Default::default()
            };
            assert_eq!(p.event(), ev, "{name}");
        }
    }

    #[tokio::test]
    async fn forward_to_nothing_is_an_error_not_a_panic() {
        // Port 1 is never listening.
        assert!(forward("http://127.0.0.1:1/hook", b"{}".to_vec())
            .await
            .is_err());
    }
}
