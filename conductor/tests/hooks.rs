//! The loopback hook endpoint: session lookup, 204s and state transitions.

use std::path::Path;
use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use conductor::hooks::{router, HookState};
use conductor::registry::{GroupKey, GroupState, Registry, SystemClock};
use conductor::tmux::{SessionRuntime, TmuxError};
use sqlx::PgPool;
use tower::ServiceExt;

struct NoTmux;

#[async_trait::async_trait]
impl SessionRuntime for NoTmux {
    async fn start(
        &self,
        _: &str,
        _: &Path,
        _: &[String],
        _: &[(String, String)],
    ) -> Result<(), TmuxError> {
        Ok(())
    }
    async fn exists(&self, _: &str) -> bool {
        true
    }
    async fn pane_pid(&self, _: &str) -> Option<u32> {
        Some(99)
    }
    async fn clients(&self, _: &str) -> Vec<String> {
        vec![]
    }
    async fn list_sessions(&self) -> Result<Vec<String>, TmuxError> {
        Ok(vec![])
    }
    async fn deliver(&self, _: &str, _: &str) -> Result<(), TmuxError> {
        Ok(())
    }
    async fn interrupt(&self, _: &str) -> Result<(), TmuxError> {
        Ok(())
    }
    async fn kill(&self, _: &str) -> Result<(), TmuxError> {
        Ok(())
    }
    async fn detach_clients(&self, _: &str) -> Result<(), TmuxError> {
        Ok(())
    }
    fn attach_command(&self, _: &str, _: bool) -> Vec<String> {
        vec![]
    }
}

async fn post(app: &axum::Router, body: String) -> StatusCode {
    app.clone()
        .oneshot(
            Request::post("/hook")
                .body(Body::from(body))
                .expect("request"),
        )
        .await
        .expect("response")
        .status()
}

#[sqlx::test(migrations = "./migrations")]
async fn hooks_drive_the_group_state(pool: PgPool) {
    let registry = Registry::new(pool.clone(), Arc::new(SystemClock), 7);
    let key = GroupKey::Named("grp".into());
    let mut txn = registry.begin().await.unwrap();
    let (session, _) = txn.session_for_dispatch(&key).await.unwrap();
    txn.commit().await.unwrap();
    sqlx::query("UPDATE session_groups SET state = $2 WHERE slug = $1")
        .bind(key.slug())
        .bind(sqlx::types::Json(GroupState::Starting {
            since: chrono::Utc::now(),
        }))
        .execute(&pool)
        .await
        .unwrap();

    let app = router(HookState {
        registry: registry.clone(),
        runtime: Arc::new(NoTmux),
        wake: Arc::new(tokio::sync::Notify::new()),
        events: Arc::new(tokio::sync::Notify::new()),
    });
    let hook = |name: &str, id: &str| {
        format!(r#"{{"hook_event_name":"{name}","session_id":"{id}","cwd":"/x","extra":1}}"#)
    };
    let state = || async { registry.group_by_slug("grp").await.unwrap().unwrap().state };

    assert_eq!(
        post(&app, hook("SessionStart", &session.to_string())).await,
        StatusCode::NO_CONTENT
    );
    assert!(matches!(
        state().await,
        GroupState::Ready { pane_pid: 99, .. }
    ));
    post(&app, hook("UserPromptSubmit", &session.to_string())).await;
    assert!(matches!(
        state().await,
        GroupState::Busy {
            delivered_event: None,
            ..
        }
    ));
    post(&app, hook("Stop", &session.to_string())).await;
    assert!(matches!(state().await, GroupState::Ready { .. }));

    // Unknown session, unknown event and non-UUID ids are all quietly accepted.
    let other = uuid::Uuid::new_v4().to_string();
    assert_eq!(
        post(&app, hook("Stop", &other)).await,
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        post(&app, hook("Notification", &session.to_string())).await,
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        post(&app, hook("Stop", "not-a-uuid")).await,
        StatusCode::NO_CONTENT
    );
    assert_eq!(post(&app, "nope".into()).await, StatusCode::BAD_REQUEST);

    post(&app, hook("SessionEnd", &session.to_string())).await;
    assert_eq!(state().await, GroupState::NoSession);
    // A stray Stop after the session ended changes nothing.
    post(&app, hook("Stop", &session.to_string())).await;
    assert_eq!(state().await, GroupState::NoSession);
}
