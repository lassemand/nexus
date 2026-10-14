//! Automatic closing of a group once every issue in its set is complete.

mod common;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use common::{linear_event, Harness};
use conductor::linear::{ClosedIssue, ClosedKind};
use conductor::linear_api::{IssueSetSource, LinearApiError, SetIssue};
use conductor::registry::GroupKey;
use conductor::{EventSink, InboundEvent};
use sqlx::PgPool;

const PATIENCE: Duration = Duration::from_secs(60);

/// In-memory set whose answer a test can change.
struct FakeSource(Mutex<Result<Vec<(String, String)>, ()>>);

impl FakeSource {
    fn with(states: &[&str]) -> Arc<Self> {
        Arc::new(FakeSource(Mutex::new(Ok(states
            .iter()
            .enumerate()
            .map(|(i, s)| (format!("NEX-{i}"), s.to_string()))
            .collect()))))
    }
    fn failing() -> Arc<Self> {
        Arc::new(FakeSource(Mutex::new(Err(()))))
    }
}

#[async_trait::async_trait]
impl IssueSetSource for FakeSource {
    async fn issues_in_set(&self, _key: &GroupKey) -> Result<Vec<SetIssue>, LinearApiError> {
        match &*self.0.lock().unwrap() {
            Ok(v) => Ok(v
                .iter()
                .map(|(i, s)| SetIssue {
                    identifier: i.clone(),
                    state_type: s.clone(),
                })
                .collect()),
            Err(()) => Err(LinearApiError::Status(500)),
        }
    }
}

fn closed(group: &str, delivery: &str) -> InboundEvent {
    InboundEvent::LinearIssueClosed {
        issue: ClosedIssue {
            identifier: "NEX-1".into(),
            session_labels: vec![group.into()],
            kind: ClosedKind::Completed,
        },
        delivery_id: Some(delivery.into()),
    }
}

/// Runs one task in `name`'s group so the group and worktree exist and are idle.
async fn seeded(h: &Harness, sink: &conductor::dispatcher::DispatchSink, name: &str) -> GroupKey {
    sink.submit(linear_event("NEX-1", name, Some("d1")));
    assert!(
        h.wait_for_runs(1, PATIENCE).await,
        "first run never finished"
    );
    let key = GroupKey::Named(name.into());
    assert!(
        h.wait_for_state(&key, PATIENCE, |s| *s
            == conductor::registry::GroupState::Idle)
            .await
    );
    key
}

#[sqlx::test(migrations = "./migrations")]
async fn complete_set_closes_the_group_and_audits_it(pool: PgPool) {
    let h = Harness::new(pool).with_fake(20, 0, None, false);
    let (_d, sink) = h.start_with_source(FakeSource::with(&["completed", "canceled"]));
    let key = seeded(&h, &sink, "done").await;
    let worktree = h.group(&key).await.expect("group").worktree;
    assert!(worktree.exists());

    sink.submit(closed("done", "c1"));

    let deadline = tokio::time::Instant::now() + PATIENCE;
    while h.group(&key).await.is_some() && tokio::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(h.group(&key).await.is_none(), "group should be closed");
    assert!(!worktree.exists(), "worktree should be removed");

    let audit = h.registry.recent_closed(10).await.expect("audit");
    assert_eq!(audit.len(), 1);
    assert_eq!(audit[0].slug, key.slug());
    assert_eq!(audit[0].mode, "auto");
}

#[sqlx::test(migrations = "./migrations")]
async fn open_issue_keeps_the_group(pool: PgPool) {
    let h = Harness::new(pool).with_fake(20, 0, None, false);
    let (_d, sink) = h.start_with_source(FakeSource::with(&["completed", "backlog"]));
    let key = seeded(&h, &sink, "partial").await;

    sink.submit(closed("partial", "c1"));

    assert!(
        h.wait_for_group(&key, PATIENCE, |g| g.close_requested_at.is_none())
            .await,
        "request should be withdrawn once the set is found incomplete"
    );
    assert!(h.group(&key).await.is_some());
    assert!(h.registry.recent_closed(10).await.unwrap().is_empty());
}

#[sqlx::test(migrations = "./migrations")]
async fn linear_failure_never_closes(pool: PgPool) {
    let h = Harness::new(pool).with_fake(20, 0, None, false);
    let (_d, sink) = h.start_with_source(FakeSource::failing());
    let key = seeded(&h, &sink, "flaky").await;

    sink.submit(closed("flaky", "c1"));
    tokio::time::sleep(Duration::from_secs(1)).await;

    assert!(h.group(&key).await.is_some());
    assert!(h.registry.recent_closed(10).await.unwrap().is_empty());
}

#[sqlx::test(migrations = "./migrations")]
async fn closed_event_for_unknown_set_creates_nothing(pool: PgPool) {
    let h = Harness::new(pool).with_fake(20, 0, None, false);
    let (_d, sink) = h.start_with_source(FakeSource::with(&["completed"]));

    sink.submit(closed("never-ran", "c1"));
    tokio::time::sleep(Duration::from_millis(500)).await;

    assert!(h
        .group(&GroupKey::Named("never-ran".into()))
        .await
        .is_none());
    assert!(h.runs().is_empty());
}
