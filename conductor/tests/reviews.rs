//! Automatic reviews end to end through the dispatcher: triggers, the review
//! set and its own slot, the verdict and its side effects, against fake
//! Linear and GitHub clients and the fake tmux/Claude.

mod common;

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use common::{git, linear_event, Harness};
use conductor::dispatcher::ensure_detached_worktree;
use conductor::github::ReviewCommand;
use conductor::github_api::GithubApiError;
use conductor::linear::IssueStateChange;
use conductor::linear_api::LinearApiError;
use conductor::registry::{GroupKey, GroupState};
use conductor::review::ci_gate::CheckRun;
use conductor::review::{store, PullRequest, ReviewGithub, ReviewIssue, ReviewLinear, Trigger};
use conductor::sessions::SessionsCommand;
use conductor::{EventSink, InboundEvent};
use sqlx::PgPool;

const PATIENCE: Duration = Duration::from_secs(60);
const REPO: &str = "lassemand/nexus";
const ISSUE_ID: &str = "uuid-nex-1";

#[derive(Default)]
struct FakeLinear {
    issues: Mutex<Vec<ReviewIssue>>,
    comments: Mutex<Vec<(String, String)>>,
    moved: Mutex<Vec<String>>,
    moves: Mutex<Vec<String>>,
    fail_writes: AtomicBool,
    fail_move: AtomicBool,
}

impl FakeLinear {
    fn comments(&self) -> Vec<String> {
        self.comments
            .lock()
            .unwrap()
            .iter()
            .map(|(_, b)| b.clone())
            .collect()
    }
}

#[async_trait::async_trait]
impl ReviewLinear for FakeLinear {
    async fn issue(&self, id: &str) -> Result<Option<ReviewIssue>, LinearApiError> {
        Ok(self
            .issues
            .lock()
            .unwrap()
            .iter()
            .find(|i| i.id == id || i.identifier.eq_ignore_ascii_case(id))
            .cloned())
    }

    async fn comment(&self, issue_id: &str, body: &str) -> Result<(), LinearApiError> {
        if self.fail_writes.load(Ordering::SeqCst) {
            return Err(LinearApiError::Unauthorized);
        }
        self.comments
            .lock()
            .unwrap()
            .push((issue_id.to_string(), body.to_string()));
        Ok(())
    }

    async fn move_to_todo(&self, issue: &ReviewIssue) -> Result<(), LinearApiError> {
        if self.fail_writes.load(Ordering::SeqCst) || self.fail_move.load(Ordering::SeqCst) {
            return Err(LinearApiError::Unauthorized);
        }
        self.moved.lock().unwrap().push(issue.id.clone());
        Ok(())
    }

    async fn move_to_state(&self, issue: &ReviewIssue, state: &str) -> Result<(), LinearApiError> {
        if self.fail_writes.load(Ordering::SeqCst) {
            return Err(LinearApiError::Unauthorized);
        }
        self.moves.lock().unwrap().push(state.to_string());
        for i in self.issues.lock().unwrap().iter_mut() {
            if i.id == issue.id {
                i.state_name = state.to_string();
            }
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Default, PartialEq)]
enum Ci {
    #[default]
    Green,
    Pending,
    NoChecks,
    Failed,
    Error,
}

#[derive(Default)]
struct FakeGithub {
    prs: Mutex<HashMap<u64, PullRequest>>,
    added: Mutex<Vec<(u64, String)>>,
    removed: Mutex<Vec<(u64, String)>>,
    reactions: Mutex<Vec<(u64, String)>>,
    comments: Mutex<Vec<(u64, String)>>,
    ci: Mutex<Ci>,
    check_calls: std::sync::atomic::AtomicUsize,
    push_after_first_check: Mutex<Option<(u64, String)>>,
}

impl FakeGithub {
    fn set_ci(&self, ci: Ci) {
        *self.ci.lock().unwrap() = ci;
    }

    fn set_head(&self, number: u64, sha: &str) {
        if let Some(pr) = self.prs.lock().unwrap().get_mut(&number) {
            pr.head_sha = sha.to_string();
        }
    }
}

#[async_trait::async_trait]
impl ReviewGithub for FakeGithub {
    async fn pull_request(&self, _repo: &str, number: u64) -> Result<PullRequest, GithubApiError> {
        self.prs
            .lock()
            .unwrap()
            .get(&number)
            .cloned()
            .ok_or(GithubApiError::Status(404))
    }
    async fn add_label(&self, _repo: &str, number: u64, label: &str) -> Result<(), GithubApiError> {
        self.added.lock().unwrap().push((number, label.into()));
        Ok(())
    }
    async fn remove_label(
        &self,
        _repo: &str,
        number: u64,
        label: &str,
    ) -> Result<(), GithubApiError> {
        self.removed.lock().unwrap().push((number, label.into()));
        Ok(())
    }
    async fn react(
        &self,
        _repo: &str,
        comment_id: u64,
        content: &str,
    ) -> Result<(), GithubApiError> {
        self.reactions
            .lock()
            .unwrap()
            .push((comment_id, content.into()));
        Ok(())
    }
    async fn comment(&self, _repo: &str, number: u64, body: &str) -> Result<(), GithubApiError> {
        self.comments.lock().unwrap().push((number, body.into()));
        Ok(())
    }
    async fn check_runs(&self, _repo: &str, _sha: &str) -> Result<Vec<CheckRun>, GithubApiError> {
        self.check_calls.fetch_add(1, Ordering::SeqCst);
        let mode = *self.ci.lock().unwrap();
        let push = self.push_after_first_check.lock().unwrap().take();
        if let Some((number, sha)) = push {
            self.set_head(number, &sha);
            self.set_ci(Ci::Pending);
        }
        let run = |status: &str, conclusion: Option<&str>| CheckRun {
            name: "build".into(),
            status: status.into(),
            conclusion: conclusion.map(str::to_string),
            url: "https://github.com/lassemand/nexus/runs/1".into(),
        };
        match mode {
            Ci::Green => Ok(vec![run("completed", Some("success"))]),
            Ci::Pending => Ok(vec![run("in_progress", None)]),
            Ci::NoChecks => Ok(vec![]),
            Ci::Failed => Ok(vec![run("completed", Some("failure"))]),
            Ci::Error => Err(GithubApiError::Status(502)),
        }
    }
}

/// A harness with one issue, NEX-1, whose open PR #316 has `main`'s head.
struct Setup {
    h: Harness,
    linear: Arc<FakeLinear>,
    github: Arc<FakeGithub>,
    sha: String,
}

fn setup(h: Harness) -> Setup {
    let sha = git(&h.repo, &["rev-parse", "HEAD"]).trim().to_string();
    let linear = Arc::new(FakeLinear::default());
    linear.issues.lock().unwrap().push(ReviewIssue {
        id: ISSUE_ID.into(),
        identifier: "NEX-1".into(),
        url: "https://linear.app/nexusboard/issue/NEX-1".into(),
        state_name: "In Review".into(),
        team_id: "team-1".into(),
        attachment_urls: vec![
            format!("https://github.com/{REPO}/pull/316"),
            format!("https://github.com/{REPO}/pull/300"),
        ],
    });
    let github = Arc::new(FakeGithub::default());
    for (number, open) in [(316, true), (300, false)] {
        github.prs.lock().unwrap().insert(
            number,
            PullRequest {
                repo: REPO.into(),
                number,
                url: format!("https://github.com/{REPO}/pull/{number}"),
                open,
                head_sha: sha.clone(),
                head_ref: "feature/NEX-1-thing".into(),
                head_repo: Some(REPO.into()),
                base_repo: Some(REPO.into()),
                body: String::new(),
                author: "nexus-mcp[bot]".into(),
            },
        );
    }
    Setup {
        h,
        linear,
        github,
        sha,
    }
}

fn verdict_cmd(verdict: &str, sha: &str) -> String {
    format!(
        "mkdir -p .review && printf '%s' '{{\"verdict\":\"{verdict}\",\"prs\":[{{\"number\":316,\"head_sha\":\"{sha}\",\"review_id\":77,\"blocking\":1,\"non_blocking\":0}}],\"summary\":\"Found things.\"}}' > .review/verdict.json"
    )
}

fn entered(state: &str, delivery: &str) -> InboundEvent {
    InboundEvent::LinearIssueStateChanged {
        change: IssueStateChange {
            issue_id: ISSUE_ID.into(),
            identifier: "NEX-1".into(),
            state_name: state.into(),
            state_type: "started".into(),
        },
        delivery_id: Some(delivery.into()),
    }
}

fn review_key() -> GroupKey {
    GroupKey::review("NEX-1").expect("valid key")
}

async fn send_backs(h: &Harness) -> i32 {
    store::state(&h.pool, ISSUE_ID)
        .await
        .unwrap()
        .map(|s| s.send_backs)
        .unwrap_or(0)
}

async fn seed_send_backs(h: &Harness, n: usize) {
    for _ in 0..n {
        store::add_send_back(&h.pool, ISSUE_ID, "NEX-1")
            .await
            .unwrap();
    }
}

fn set_issue_state(linear: &FakeLinear, state: &str) {
    linear.issues.lock().unwrap()[0].state_name = state.into();
}

async fn wait_until(mut done: impl FnMut() -> bool) -> bool {
    let deadline = tokio::time::Instant::now() + PATIENCE;
    while tokio::time::Instant::now() < deadline {
        if done() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    done()
}

/// Waits until a review was delivered and its set is closed and forgotten.
async fn wait_for_closed(h: &Harness) -> bool {
    let deadline = tokio::time::Instant::now() + PATIENCE;
    while review_prompts(h).is_empty() {
        if tokio::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    loop {
        let gone = h.group(&review_key()).await.is_none();
        let no_run = store::run(&h.pool, ISSUE_ID).await.unwrap().is_none();
        if gone && no_run {
            return true;
        }
        if tokio::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

fn review_prompts(h: &Harness) -> Vec<String> {
    h.fake
        .delivered
        .lock()
        .unwrap()
        .iter()
        .filter(|(slug, _)| slug == "review-nex-1")
        .map(|(_, text)| text.clone())
        .collect()
}

#[sqlx::test(migrations = "./migrations")]
async fn changes_requested_sends_the_issue_back_to_todo(pool: PgPool) {
    let s = setup(Harness::new(pool).with_fake(50, 0, None, false));
    let cmd = format!(
        "git rev-parse HEAD > ../review-head.txt && {}",
        verdict_cmd("changes_requested", &s.sha)
    );
    s.h.fake.set_turn(50, Some(&cmd));
    let (_d, sink) = s.h.start_with_reviews(s.linear.clone(), s.github.clone());

    sink.submit(entered("In Review", "d1"));

    assert!(
        wait_until(|| !s.linear.moved.lock().unwrap().is_empty()).await,
        "no send-back"
    );
    assert!(wait_for_closed(&s.h).await, "the review set must be closed");
    assert_eq!(send_backs(&s.h).await, 1);
    let comments = s.linear.comments();
    assert_eq!(comments.len(), 1, "{comments:?}");
    assert!(
        comments[0].starts_with("Review send-back 1 of 2"),
        "{}",
        comments[0]
    );
    assert!(comments[0].contains("pull/316#pullrequestreview-77"));
    assert_eq!(*s.linear.moved.lock().unwrap(), vec![ISSUE_ID.to_string()]);

    let prompts = review_prompts(&s.h);
    assert_eq!(prompts.len(), 1);
    let prompt = &prompts[0];
    assert!(prompt.contains("Linear issue: NEX-1"));
    assert!(prompt.contains(&format!(
        "PR: number=316 url=https://github.com/{REPO}/pull/316 head_sha={}",
        s.sha
    )));
    assert!(
        !prompt.contains("number=300"),
        "a closed PR is not reviewed"
    );
    assert!(prompt.contains("Review number: 1"));
    assert!(prompt.contains("Send-backs so far: 0"));
    assert!(prompt.contains("Send-back limit: 2"));
    assert!(prompt.contains("Trigger: automatic"));
    assert!(prompt.contains("/review-nex-1/.review/verdict.json"));

    let start =
        s.h.starts()
            .into_iter()
            .find(|r| r.cwd.ends_with("review-nex-1"))
            .expect("the review ran in its own worktree");
    assert!(start.argv.contains(&"--agent=reviewer".to_string()));
    assert_eq!(start.model(), Some("opus"));
    let head = std::fs::read_to_string(s.h.worktrees.join("review-head.txt")).unwrap();
    assert_eq!(head.trim(), s.sha, "worktree at the PR head");
}

#[sqlx::test(migrations = "./migrations")]
async fn approve_comments_and_leaves_the_issue_in_review(pool: PgPool) {
    let s = setup(Harness::new(pool).with_fake(50, 0, None, false));
    s.h.fake.set_turn(50, Some(&verdict_cmd("approve", &s.sha)));
    let (_d, sink) = s.h.start_with_reviews(s.linear.clone(), s.github.clone());

    sink.submit(entered("In Review", "d1"));

    assert!(wait_for_closed(&s.h).await);
    assert!(wait_until(|| !s.linear.comments().is_empty()).await);
    assert!(s.linear.comments()[0].starts_with("Review approved"));
    assert!(s.linear.moved.lock().unwrap().is_empty());
    assert_eq!(send_backs(&s.h).await, 0);
    assert_eq!(
        *s.github.removed.lock().unwrap(),
        vec![(316, "needs-human".to_string())]
    );
    let state = store::state(&s.h.pool, ISSUE_ID).await.unwrap().unwrap();
    assert_eq!(state.reviews_total, 1);
    assert_eq!(state.last_verdict.as_deref(), Some("approve"));
}

#[sqlx::test(migrations = "./migrations")]
async fn at_the_limit_no_review_runs_and_one_notice_is_posted(pool: PgPool) {
    let s = setup(Harness::new(pool).with_fake(50, 0, None, false));
    seed_send_backs(&s.h, 2).await;
    let (_d, sink) = s.h.start_with_reviews(s.linear.clone(), s.github.clone());

    sink.submit(entered("In Review", "d1"));
    assert!(wait_until(|| !s.linear.comments().is_empty()).await);
    sink.submit(entered("In Review", "d1"));
    tokio::time::sleep(Duration::from_millis(500)).await;

    assert_eq!(
        s.linear.comments(),
        vec!["Automatic review limit reached (2 send-backs). Comment `/review` on the PR to request another review.".to_string()],
        "one notice, even when the webhook is redelivered"
    );
    assert_eq!(
        *s.github.added.lock().unwrap(),
        vec![(316, "needs-human".to_string())]
    );
    assert!(s.h.group(&review_key()).await.is_none(), "no review set");
    assert!(review_prompts(&s.h).is_empty());
    let state = store::state(&s.h.pool, ISSUE_ID).await.unwrap().unwrap();
    assert!(state.limit_notice_sent_at.is_some());
}

#[sqlx::test(migrations = "./migrations")]
async fn a_manual_review_at_the_limit_runs_and_is_not_dispatched_to_the_author(pool: PgPool) {
    let s = setup(Harness::new(pool).with_fake(50, 0, None, false));
    s.h.fake.set_turn(50, Some(&verdict_cmd("approve", &s.sha)));
    seed_send_backs(&s.h, 2).await;
    let (_d, sink) = s.h.start_with_reviews(s.linear.clone(), s.github.clone());

    sink.submit(InboundEvent::ReviewCommand {
        command: ReviewCommand {
            repo: REPO.into(),
            pr_number: 316,
            comment_id: Some(42),
            login: "lassemand".into(),
        },
        delivery_id: Some("gh-1".into()),
    });

    assert!(wait_for_closed(&s.h).await);
    assert_eq!(review_prompts(&s.h).len(), 1);
    assert!(review_prompts(&s.h)[0].contains("Trigger: manual"));
    assert_eq!(
        *s.github.reactions.lock().unwrap(),
        vec![(42, "eyes".to_string())]
    );
    let slugs: Vec<String> = s.h.fake.started.lock().unwrap().clone();
    assert_eq!(
        slugs,
        vec!["review-nex-1".to_string()],
        "nothing reached an author set"
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn a_review_comment_router_path_and_the_bot_filter(pool: PgPool) {
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    let s = setup(Harness::new(pool).with_fake(50, 0, None, false));
    s.h.fake.set_turn(50, Some(&verdict_cmd("approve", &s.sha)));
    let (_d, sink) = s.h.start_with_reviews(s.linear.clone(), s.github.clone());
    let router = conductor::http::router(conductor::http::AppState {
        sink: Arc::new(sink),
        github_webhook_secret: None,
        watched_github_users: vec!["lassemand".into(), "annette-review[bot]".into()],
        shutting_down: Arc::clone(&s.h.shutting_down),
        review_bot_login: Some("annette-review[bot]".into()),
    });
    let post = |login: &str, body: &str, delivery: &str| {
        let payload = serde_json::json!({
            "action": "created",
            "issue": { "number": 316, "pull_request": { "url": "u" } },
            "comment": { "id": 7, "body": body, "user": { "login": login } },
            "repository": { "full_name": REPO }
        });
        Request::builder()
            .method("POST")
            .uri("/webhook/github")
            .header("X-GitHub-Event", "issue_comment")
            .header("X-GitHub-Delivery", delivery)
            .body(Body::from(payload.to_string()))
            .unwrap()
    };

    router
        .clone()
        .oneshot(post("annette-review[bot]", "Please fix this", "g1"))
        .await
        .unwrap();
    router
        .clone()
        .oneshot(post("stranger", "/review", "g2"))
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(
        s.h.registry.snapshot().await.unwrap().is_empty(),
        "nothing dispatched"
    );

    router
        .clone()
        .oneshot(post("lassemand", "/review", "g3"))
        .await
        .unwrap();
    assert!(wait_for_closed(&s.h).await);
    assert_eq!(review_prompts(&s.h).len(), 1);
    assert_eq!(
        *s.h.fake.started.lock().unwrap(),
        vec!["review-nex-1".to_string()]
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn re_requesting_the_reviewer_app_queues_a_manual_review(pool: PgPool) {
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    let s = setup(Harness::new(pool).with_fake(50, 0, None, false));
    s.h.fake.set_turn(50, Some(&verdict_cmd("approve", &s.sha)));
    let (_d, sink) = s.h.start_with_reviews(s.linear.clone(), s.github.clone());
    let router = conductor::http::router(conductor::http::AppState {
        sink: Arc::new(sink),
        github_webhook_secret: None,
        watched_github_users: vec!["lassemand".into()],
        shutting_down: Arc::clone(&s.h.shutting_down),
        review_bot_login: Some("annette-review[bot]".into()),
    });
    let post = |reviewer: &str, sender: &str, delivery: &str| {
        let payload = serde_json::json!({
            "action": "review_requested",
            "number": 316,
            "requested_reviewer": { "login": reviewer, "type": "Bot" },
            "sender": { "login": sender },
            "repository": { "full_name": REPO }
        });
        Request::builder()
            .method("POST")
            .uri("/webhook/github")
            .header("X-GitHub-Event", "pull_request")
            .header("X-GitHub-Delivery", delivery)
            .body(Body::from(payload.to_string()))
            .unwrap()
    };

    router
        .clone()
        .oneshot(post("someone-else", "lassemand", "p1"))
        .await
        .unwrap();
    router
        .clone()
        .oneshot(post("annette-review[bot]", "stranger", "p2"))
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(
        s.h.registry.snapshot().await.unwrap().is_empty(),
        "nothing queued"
    );

    router
        .clone()
        .oneshot(post("annette-review[bot]", "lassemand", "p3"))
        .await
        .unwrap();
    assert!(wait_for_closed(&s.h).await);
    assert_eq!(review_prompts(&s.h).len(), 1);
    assert!(review_prompts(&s.h)[0].contains("Trigger: manual"));
    assert!(
        s.github.reactions.lock().unwrap().is_empty(),
        "no comment to react to"
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn a_missing_verdict_fails_the_review_without_a_send_back(pool: PgPool) {
    let s = setup(Harness::new(pool).with_fake(50, 0, None, false));
    let (_d, sink) = s.h.start_with_reviews(s.linear.clone(), s.github.clone());

    sink.submit(entered("In Review", "d1"));

    assert!(wait_for_closed(&s.h).await);
    assert!(wait_until(|| !s.linear.comments().is_empty()).await);
    assert_eq!(
        s.linear.comments(),
        vec!["Automatic review failed: the reviewer wrote no verdict file. Comment `/review` on the PR to retry.".to_string()]
    );
    assert!(s.linear.moved.lock().unwrap().is_empty());
    assert_eq!(send_backs(&s.h).await, 0);
}

#[sqlx::test(migrations = "./migrations")]
async fn a_review_that_runs_too_long_is_interrupted_and_failed(pool: PgPool) {
    let mut h = Harness::new(pool).with_fake(5_000, 0, None, false);
    h.config.review.timeout = Duration::from_millis(500);
    let s = setup(h);
    let (_d, sink) = s.h.start_with_reviews(s.linear.clone(), s.github.clone());

    sink.submit(entered("In Review", "d1"));

    assert!(wait_for_closed(&s.h).await);
    assert!(s
        .h
        .fake
        .interrupted
        .lock()
        .unwrap()
        .contains(&"review-nex-1".to_string()));
    let comments = s.linear.comments();
    assert_eq!(comments.len(), 1);
    assert!(
        comments[0].starts_with("Automatic review failed: the review took longer"),
        "{}",
        comments[0]
    );
    assert_eq!(send_backs(&s.h).await, 0);
}

#[sqlx::test(migrations = "./migrations")]
async fn moving_the_issue_to_done_mid_review_interrupts_it(pool: PgPool) {
    let s = setup(Harness::new(pool).with_fake(10_000, 0, None, false));
    s.h.fake
        .set_turn(10_000, Some(&verdict_cmd("changes_requested", &s.sha)));
    let (_d, sink) = s.h.start_with_reviews(s.linear.clone(), s.github.clone());

    sink.submit(entered("In Review", "d1"));
    assert!(
        s.h.wait_for_state(&review_key(), PATIENCE, |st| matches!(
            st,
            GroupState::Busy { .. }
        ))
        .await
    );
    sink.submit(entered("Done", "d2"));

    assert!(wait_for_closed(&s.h).await);
    assert!(s
        .h
        .fake
        .interrupted
        .lock()
        .unwrap()
        .contains(&"review-nex-1".to_string()));
    assert!(s.linear.comments().is_empty());
    assert!(s.linear.moved.lock().unwrap().is_empty());
    assert_eq!(send_backs(&s.h).await, 0);
}

#[sqlx::test(migrations = "./migrations")]
async fn a_new_head_discards_the_verdict_and_queues_one_new_review(pool: PgPool) {
    let s = setup(Harness::new(pool).with_fake(800, 0, None, false));
    s.h.fake
        .set_turn(800, Some(&verdict_cmd("changes_requested", &s.sha)));
    std::fs::write(s.h.repo.join("NEW.md"), "new\n").unwrap();
    git(&s.h.repo, &["add", "NEW.md"]);
    git(&s.h.repo, &["commit", "-m", "new head"]);
    git(
        &s.h.repo,
        &["push", "origin", "HEAD:refs/heads/feature/NEX-1-thing"],
    );
    let new_sha = git(&s.h.repo, &["rev-parse", "HEAD"]).trim().to_string();
    let (_d, sink) = s.h.start_with_reviews(s.linear.clone(), s.github.clone());

    sink.submit(entered("In Review", "d1"));
    assert!(
        s.h.wait_for_state(&review_key(), PATIENCE, |st| matches!(
            st,
            GroupState::Busy { .. }
        ))
        .await
    );
    s.github.set_head(316, &new_sha);
    s.h.fake
        .set_turn(50, Some(&verdict_cmd("approve", &new_sha)));

    assert!(
        wait_until(|| review_prompts(&s.h).len() >= 2).await,
        "a second review"
    );
    assert!(review_prompts(&s.h)[1].contains(&format!("head_sha={new_sha}")));
    assert!(wait_until(|| !s.linear.comments().is_empty()).await);
    assert!(wait_for_closed(&s.h).await);
    assert!(
        s.linear.moved.lock().unwrap().is_empty(),
        "the stale verdict did nothing"
    );
    assert_eq!(send_backs(&s.h).await, 0);
    assert_eq!(review_prompts(&s.h).len(), 2, "exactly one new review");
    assert!(s.linear.comments()[0].starts_with("Review approved"));
}

#[sqlx::test(migrations = "./migrations")]
async fn two_triggers_for_one_issue_make_one_review(pool: PgPool) {
    let s = setup(Harness::new(pool).with_fake(1_500, 0, None, false));
    s.h.fake
        .set_turn(1_500, Some(&verdict_cmd("approve", &s.sha)));
    let (_d, sink) = s.h.start_with_reviews(s.linear.clone(), s.github.clone());

    sink.submit(entered("In Review", "d1"));
    sink.submit(entered("In Review", "d2"));
    sink.submit(InboundEvent::ReviewCommand {
        command: ReviewCommand {
            repo: REPO.into(),
            pr_number: 316,
            comment_id: Some(1),
            login: "lassemand".into(),
        },
        delivery_id: Some("g1".into()),
    });

    assert!(wait_for_closed(&s.h).await);
    assert_eq!(review_prompts(&s.h).len(), 1);
    assert_eq!(s.linear.comments().len(), 1);
}

#[sqlx::test(migrations = "./migrations")]
async fn a_review_has_its_own_slot_when_implementation_slots_are_full(pool: PgPool) {
    let s = setup(
        Harness::new(pool)
            .with_fake(3_000, 0, None, false)
            .with_max_sessions(1),
    );
    let (_d, sink) = s.h.start_with_reviews(s.linear.clone(), s.github.clone());
    let author = GroupKey::Named("author".into());

    sink.submit(linear_event("NEX-2", "author", Some("t1")));
    assert!(
        s.h.wait_for_state(&author, PATIENCE, |st| matches!(
            st,
            GroupState::Busy { .. }
        ))
        .await
    );
    sink.submit(entered("In Review", "d1"));

    assert!(
        s.h.wait_for_state(&review_key(), PATIENCE, |st| matches!(
            st,
            GroupState::Busy { .. }
        ))
        .await,
        "the review must start while the implementation slot is taken"
    );
    assert!(matches!(
        s.h.state_of(&author).await,
        Some(GroupState::Busy { .. })
    ));
}

#[sqlx::test(migrations = "./migrations")]
async fn an_issue_with_no_open_pr_gets_a_comment_and_no_review(pool: PgPool) {
    let s = setup(Harness::new(pool).with_fake(50, 0, None, false));
    s.github.prs.lock().unwrap().get_mut(&316).unwrap().open = false;
    let (_d, sink) = s.h.start_with_reviews(s.linear.clone(), s.github.clone());

    sink.submit(entered("In Review", "d1"));

    assert!(wait_until(|| !s.linear.comments().is_empty()).await);
    assert!(s.linear.comments()[0].contains("no open pull request"));
    assert!(s.h.group(&review_key()).await.is_none());
}

#[sqlx::test(migrations = "./migrations")]
async fn a_send_back_whose_linear_writes_keep_failing_counts_as_failed(pool: PgPool) {
    let s = setup(Harness::new(pool).with_fake(50, 0, None, false));
    s.h.fake
        .set_turn(50, Some(&verdict_cmd("changes_requested", &s.sha)));
    s.linear.fail_writes.store(true, Ordering::SeqCst);
    let (_d, sink) = s.h.start_with_reviews(s.linear.clone(), s.github.clone());

    sink.submit(entered("In Review", "d1"));

    assert!(wait_for_closed(&s.h).await);
    assert_eq!(send_backs(&s.h).await, 0, "no send-back counted");
    let state = store::state(&s.h.pool, ISSUE_ID).await.unwrap().unwrap();
    assert_eq!(state.last_verdict.as_deref(), Some("failed"));
}

#[sqlx::test(migrations = "./migrations")]
async fn an_automatic_trigger_for_an_issue_no_longer_in_review_starts_nothing(pool: PgPool) {
    let s = setup(Harness::new(pool).with_fake(50, 0, None, false));
    set_issue_state(&s.linear, "Done");
    let (_d, sink) = s.h.start_with_reviews(s.linear.clone(), s.github.clone());

    sink.submit(entered("In Review", "d1"));
    tokio::time::sleep(Duration::from_millis(500)).await;

    assert!(s.h.group(&review_key()).await.is_none(), "no review set");
    assert!(store::run(&s.h.pool, ISSUE_ID).await.unwrap().is_none());
    assert!(s.linear.comments().is_empty());
    assert!(s.linear.moved.lock().unwrap().is_empty());
}

#[sqlx::test(migrations = "./migrations")]
async fn a_verdict_for_an_issue_that_left_review_unseen_moves_nothing(pool: PgPool) {
    let s = setup(Harness::new(pool).with_fake(800, 0, None, false));
    s.h.fake
        .set_turn(800, Some(&verdict_cmd("changes_requested", &s.sha)));
    let (_d, sink) = s.h.start_with_reviews(s.linear.clone(), s.github.clone());

    sink.submit(entered("In Review", "d1"));
    assert!(
        s.h.wait_for_state(&review_key(), PATIENCE, |st| matches!(
            st,
            GroupState::Busy { .. }
        ))
        .await
    );
    set_issue_state(&s.linear, "Done");

    assert!(wait_for_closed(&s.h).await);
    assert!(
        s.linear.moved.lock().unwrap().is_empty(),
        "a Done issue must stay Done"
    );
    assert!(s.linear.comments().is_empty());
    assert_eq!(send_backs(&s.h).await, 0);
    let state = store::state(&s.h.pool, ISSUE_ID).await.unwrap().unwrap();
    assert_eq!(state.last_verdict.as_deref(), Some("discarded"));
}

#[sqlx::test(migrations = "./migrations")]
async fn a_reused_review_worktree_is_moved_to_the_commit_under_review(pool: PgPool) {
    let h = Harness::new(pool);
    let first = git(&h.repo, &["rev-parse", "HEAD"]).trim().to_string();
    let path = ensure_detached_worktree(&h.repo, &h.worktrees, "review-nex-9", &first)
        .await
        .unwrap();
    std::fs::write(h.repo.join("NEXT.md"), "next\n").unwrap();
    git(&h.repo, &["add", "NEXT.md"]);
    git(&h.repo, &["commit", "-m", "next"]);
    git(&h.repo, &["push", "origin", "HEAD:refs/heads/feature/next"]);
    let second = git(&h.repo, &["rev-parse", "HEAD"]).trim().to_string();
    assert_ne!(first, second);
    assert_eq!(git(&path, &["rev-parse", "HEAD"]).trim(), first);

    let again = ensure_detached_worktree(&h.repo, &h.worktrees, "review-nex-9", &second)
        .await
        .unwrap();

    assert_eq!(again, path);
    assert_eq!(git(&path, &["rev-parse", "HEAD"]).trim(), second);
    assert!(path.join("NEXT.md").exists());
}

#[sqlx::test(migrations = "./migrations")]
async fn a_review_waiting_for_a_slot_is_not_failed_as_stuck(pool: PgPool) {
    let s = setup(Harness::new(pool).with_fake(50, 0, None, false));
    let (_d, sink) = s.h.start_with_reviews(s.linear.clone(), s.github.clone());
    let issue = ReviewIssue {
        id: "uuid-nex-2".into(),
        identifier: "NEX-2".into(),
        url: "https://linear.app/nexusboard/issue/NEX-2".into(),
        state_name: "In Review".into(),
        team_id: "team-1".into(),
        attachment_urls: vec![format!("https://github.com/{REPO}/pull/316")],
    };
    s.linear.issues.lock().unwrap().push(issue);

    sink.submit(entered("In Review", "d1"));
    sink.submit(InboundEvent::LinearIssueStateChanged {
        change: IssueStateChange {
            issue_id: "uuid-nex-2".into(),
            identifier: "NEX-2".into(),
            state_name: "In Review".into(),
            state_type: "started".into(),
        },
        delivery_id: Some("d2".into()),
    });

    assert!(wait_until(|| s.linear.comments().len() >= 2).await);
    let comments = s.linear.comments();
    assert!(
        comments.iter().all(|c| !c.contains("did not start")),
        "{comments:?}"
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn attaching_to_a_queued_review_set_starts_the_review_agent_and_model(pool: PgPool) {
    let s = setup(Harness::new(pool).with_fake(50, 0, None, false));
    let (_d, _sink) = s.h.start_with_reviews(s.linear.clone(), s.github.clone());
    {
        let mut txn = s.h.registry.begin().await.unwrap();
        txn.session_for_dispatch(&review_key()).await.unwrap();
        store::set_worktree_ref(txn.conn(), "review-nex-1", &s.sha)
            .await
            .unwrap();
        txn.commit().await.unwrap();
    }

    let terminal = common::FakeTerminal::interactive();
    let (code, text) =
        s.h.sessions_on(
            SessionsCommand::Attach {
                group: "review-nex-1".into(),
                read_only: false,
                force: false,
            },
            &terminal,
        )
        .await;

    assert_eq!(code, 0, "{text}");
    let argv =
        s.h.fake
            .session_argv("review-nex-1")
            .expect("the review session was started");
    assert!(argv.contains(&"--agent=reviewer".to_string()), "{argv:?}");
    assert!(
        argv.windows(2).any(|w| w == ["--model", "opus"]),
        "{argv:?}"
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn a_failed_move_leaves_the_send_back_counted_with_its_comment(pool: PgPool) {
    let s = setup(Harness::new(pool).with_fake(50, 0, None, false));
    s.h.fake
        .set_turn(50, Some(&verdict_cmd("changes_requested", &s.sha)));
    s.linear.fail_move.store(true, Ordering::SeqCst);
    let (_d, sink) = s.h.start_with_reviews(s.linear.clone(), s.github.clone());

    sink.submit(entered("In Review", "d1"));
    assert!(wait_for_closed(&s.h).await);
    assert!(wait_until(|| s.linear.comments().len() >= 2).await);

    assert!(s.linear.moved.lock().unwrap().is_empty());
    assert_eq!(send_backs(&s.h).await, 1, "the posted comment is counted");
    let state = store::state(&s.h.pool, ISSUE_ID).await.unwrap().unwrap();
    assert_eq!(state.last_verdict.as_deref(), Some("failed"));

    s.linear.fail_move.store(false, Ordering::SeqCst);
    sink.submit(entered("In Review", "d2"));
    assert!(wait_until(|| !s.linear.moved.lock().unwrap().is_empty()).await);
    assert!(wait_for_closed(&s.h).await);
    let comments = s.linear.comments();
    assert!(comments[0].starts_with("Review send-back 1 of 2"));
    assert!(
        comments
            .iter()
            .any(|c| c.starts_with("Review send-back 2 of 2")),
        "the second send-back must not repeat 1 of 2: {comments:?}"
    );
    assert_eq!(send_backs(&s.h).await, 2);
}

#[sqlx::test(migrations = "./migrations")]
async fn a_review_never_closes_a_session_label_set_that_shares_its_name(pool: PgPool) {
    let s = setup(Harness::new(pool).with_fake(50, 0, None, false));
    let named = GroupKey::Named("review-nex-1".into());
    {
        let mut txn = s.h.registry.begin().await.unwrap();
        txn.session_for_dispatch(&named).await.unwrap();
        txn.commit().await.unwrap();
    }
    let (_d, sink) = s.h.start_with_reviews(s.linear.clone(), s.github.clone());

    sink.submit(entered("In Review", "d1"));
    assert!(wait_until(|| !s.linear.comments().is_empty()).await);

    let group = s.h.group(&named).await.expect("the named set survives");
    assert_eq!(group.key, named);
    assert!(store::run(&s.h.pool, ISSUE_ID).await.unwrap().is_none());
    assert!(s.linear.comments()[0].contains("is not a review set"));
}

#[sqlx::test(migrations = "./migrations")]
async fn pull_requests_in_another_repository_are_commented_on_and_skipped(pool: PgPool) {
    let mut h = Harness::new(pool).with_fake(50, 0, None, false);
    h.config.review.repo = Some(REPO.into());
    let s = setup(h);
    s.linear.issues.lock().unwrap()[0]
        .attachment_urls
        .insert(0, "https://github.com/other/repo/pull/5".into());
    s.h.fake.set_turn(50, Some(&verdict_cmd("approve", &s.sha)));
    let (_d, sink) = s.h.start_with_reviews(s.linear.clone(), s.github.clone());

    sink.submit(entered("In Review", "d1"));

    assert!(wait_for_closed(&s.h).await);
    assert!(wait_until(|| s.linear.comments().len() >= 2).await);
    let comments = s.linear.comments();
    assert!(
        comments
            .iter()
            .any(|c| c.contains("other/repo#5") && c.contains(REPO)),
        "{comments:?}"
    );
    assert_eq!(review_prompts(&s.h).len(), 1);
}

#[sqlx::test(migrations = "./migrations")]
async fn the_limit_notice_is_not_repeated_until_a_send_back_changes_the_count(pool: PgPool) {
    let s = setup(Harness::new(pool).with_fake(50, 0, None, false));
    seed_send_backs(&s.h, 2).await;
    let (_d, sink) = s.h.start_with_reviews(s.linear.clone(), s.github.clone());

    sink.submit(entered("In Review", "d1"));
    assert!(wait_until(|| !s.linear.comments().is_empty()).await);
    sink.submit(entered("In Review", "d2"));
    sink.submit(entered("In Review", "d3"));
    tokio::time::sleep(Duration::from_millis(500)).await;

    assert_eq!(
        s.linear.comments().len(),
        1,
        "one notice per send-back count"
    );
    assert_eq!(s.github.added.lock().unwrap().len(), 1);

    store::reset(&s.h.pool, "NEX-1").await.unwrap();
    seed_send_backs(&s.h, 2).await;
    sink.submit(entered("In Review", "d4"));
    assert!(wait_until(|| s.linear.comments().len() == 2).await);
}

#[sqlx::test(migrations = "./migrations")]
async fn a_run_keeps_the_pull_request_a_review_command_named(pool: PgPool) {
    let h = Harness::new(pool);
    let run = store::ReviewRun {
        issue_id: ISSUE_ID.into(),
        identifier: "NEX-1".into(),
        issue_url: "u".into(),
        group_slug: "review-nex-1".into(),
        trigger: Trigger::Manual,
        review_number: 1,
        prs: vec![],
        created_at: chrono::Utc::now(),
        cancelled_at: None,
        extra: Some((REPO.into(), 316)),
    };
    let mut conn = h.pool.acquire().await.unwrap();
    assert!(store::insert_run(&mut conn, &run).await.unwrap());
    drop(conn);

    let read = store::run(&h.pool, ISSUE_ID).await.unwrap().unwrap();
    assert_eq!(read.extra, Some((REPO.to_string(), 316)));
}

fn checks_done(delivery: &str, sha: &str) -> InboundEvent {
    InboundEvent::GithubChecksCompleted {
        repo: REPO.into(),
        head_sha: sha.into(),
        delivery_id: Some(delivery.into()),
    }
}

async fn watch_of(h: &Harness) -> Option<store::CiWatch> {
    store::watch(&h.pool, ISSUE_ID).await.unwrap()
}

async fn assert_no_review(s: &Setup) {
    assert!(s.h.group(&review_key()).await.is_none(), "no review set");
    assert!(store::run(&s.h.pool, ISSUE_ID).await.unwrap().is_none());
    assert!(review_prompts(&s.h).is_empty());
    assert!(!s
        .h
        .fake
        .started
        .lock()
        .unwrap()
        .contains(&"review-nex-1".to_string()));
}

#[sqlx::test(migrations = "./migrations")]
async fn entering_review_with_ci_running_bounces_to_in_progress_and_watches(pool: PgPool) {
    let s = setup(Harness::new(pool).with_fake(50, 0, None, false));
    s.github.set_ci(Ci::Pending);
    let (_d, sink) = s.h.start_with_reviews(s.linear.clone(), s.github.clone());

    sink.submit(entered("In Review", "d1"));

    assert!(wait_until(|| !s.linear.comments().is_empty()).await);
    assert_eq!(*s.linear.moves.lock().unwrap(), vec!["In Progress"]);
    assert_eq!(
        s.linear.comments(),
        vec![format!(
            "CI not finished on {REPO} PR 316; moved back until it passes."
        )]
    );
    let watch = watch_of(&s.h).await.expect("watched");
    assert_eq!(watch.prs[0].head_sha, s.sha);
    assert_no_review(&s).await;
}

#[sqlx::test(migrations = "./migrations")]
async fn entering_review_with_no_checks_yet_counts_as_pending(pool: PgPool) {
    let s = setup(Harness::new(pool).with_fake(50, 0, None, false));
    s.github.set_ci(Ci::NoChecks);
    let (_d, sink) = s.h.start_with_reviews(s.linear.clone(), s.github.clone());
    sink.submit(entered("In Review", "d1"));
    assert!(wait_until(|| !s.linear.comments().is_empty()).await);
    assert_eq!(*s.linear.moves.lock().unwrap(), vec!["In Progress"]);
    assert!(watch_of(&s.h).await.is_some());
    assert_no_review(&s).await;
}

#[sqlx::test(migrations = "./migrations")]
async fn entering_review_with_failed_ci_goes_to_todo_without_a_send_back(pool: PgPool) {
    let s = setup(Harness::new(pool).with_fake(50, 0, None, false));
    s.github.set_ci(Ci::Failed);
    let (_d, sink) = s.h.start_with_reviews(s.linear.clone(), s.github.clone());

    sink.submit(entered("In Review", "d1"));

    assert!(wait_until(|| !s.linear.comments().is_empty()).await);
    assert_eq!(*s.linear.moved.lock().unwrap(), vec![ISSUE_ID.to_string()]);
    assert!(s.linear.moves.lock().unwrap().is_empty());
    let comment = &s.linear.comments()[0];
    assert!(comment.contains("[build](https://github.com/lassemand/nexus/runs/1)"));
    assert_eq!(send_backs(&s.h).await, 0);
    assert!(watch_of(&s.h).await.is_none());
    assert_no_review(&s).await;
}

#[sqlx::test(migrations = "./migrations")]
async fn all_green_starts_the_review(pool: PgPool) {
    let s = setup(Harness::new(pool).with_fake(50, 0, None, false));
    s.h.fake.set_turn(50, Some(&verdict_cmd("approve", &s.sha)));
    let (_d, sink) = s.h.start_with_reviews(s.linear.clone(), s.github.clone());

    sink.submit(entered("In Review", "d1"));

    assert!(wait_for_closed(&s.h).await);
    assert_eq!(review_prompts(&s.h).len(), 1);
    assert!(s.linear.moves.lock().unwrap().is_empty());
    assert!(watch_of(&s.h).await.is_none());
}

#[sqlx::test(migrations = "./migrations")]
async fn a_watched_issue_that_turns_green_is_promoted_and_reviewed_through_the_gate(pool: PgPool) {
    let s = setup(Harness::new(pool).with_fake(50, 0, None, false));
    s.h.fake.set_turn(50, Some(&verdict_cmd("approve", &s.sha)));
    s.github.set_ci(Ci::Pending);
    let (_d, sink) = s.h.start_with_reviews(s.linear.clone(), s.github.clone());
    sink.submit(entered("In Review", "d1"));
    assert!(wait_until(|| !s.linear.comments().is_empty()).await);

    s.github.set_ci(Ci::Green);
    sink.submit(checks_done("c1", &s.sha));

    assert!(
        wait_until(|| s.linear.moves.lock().unwrap().len() == 2).await,
        "promoted"
    );
    assert_eq!(
        *s.linear.moves.lock().unwrap(),
        vec!["In Progress", "In Review"]
    );
    assert!(watch_of(&s.h).await.is_none());
    sink.submit(entered("In Review", "d2"));
    assert!(wait_for_closed(&s.h).await);
    assert_eq!(review_prompts(&s.h).len(), 1);
}

#[sqlx::test(migrations = "./migrations")]
async fn two_in_review_events_while_ci_runs_move_and_comment_once(pool: PgPool) {
    let s = setup(Harness::new(pool).with_fake(50, 0, None, false));
    s.github.set_ci(Ci::Pending);
    let (_d, sink) = s.h.start_with_reviews(s.linear.clone(), s.github.clone());

    sink.submit(entered("In Review", "d1"));
    sink.submit(entered("In Review", "d2"));
    assert!(wait_until(|| !s.linear.comments().is_empty()).await);
    tokio::time::sleep(Duration::from_millis(500)).await;

    assert_eq!(*s.linear.moves.lock().unwrap(), vec!["In Progress"]);
    assert_eq!(s.linear.comments().len(), 1);
    assert_no_review(&s).await;
}

#[sqlx::test(migrations = "./migrations")]
async fn a_github_error_is_not_green_and_the_issue_is_bounced_and_watched(pool: PgPool) {
    let s = setup(Harness::new(pool).with_fake(50, 0, None, false));
    s.github.set_ci(Ci::Error);
    let (_d, sink) = s.h.start_with_reviews(s.linear.clone(), s.github.clone());

    sink.submit(entered("In Review", "d1"));

    assert!(wait_until(|| !s.linear.comments().is_empty()).await);
    assert_eq!(*s.linear.moves.lock().unwrap(), vec!["In Progress"]);
    assert!(watch_of(&s.h).await.is_some());
    assert_no_review(&s).await;
}

#[sqlx::test(migrations = "./migrations")]
async fn a_push_between_the_check_and_the_queueing_starts_no_review(pool: PgPool) {
    let s = setup(Harness::new(pool).with_fake(50, 0, None, false));
    *s.github.push_after_first_check.lock().unwrap() = Some((316, "f".repeat(40)));
    let (_d, sink) = s.h.start_with_reviews(s.linear.clone(), s.github.clone());

    sink.submit(entered("In Review", "d1"));

    assert!(wait_until(|| !s.linear.comments().is_empty()).await);
    assert_eq!(*s.linear.moves.lock().unwrap(), vec!["In Progress"]);
    let watch = watch_of(&s.h).await.expect("watched");
    assert_eq!(watch.prs[0].head_sha, "f".repeat(40));
    assert_no_review(&s).await;
}

#[sqlx::test(migrations = "./migrations")]
async fn a_new_head_on_a_watched_pull_request_resets_the_watch(pool: PgPool) {
    let s = setup(Harness::new(pool).with_fake(50, 0, None, false));
    s.github.set_ci(Ci::Pending);
    let (_d, sink) = s.h.start_with_reviews(s.linear.clone(), s.github.clone());
    sink.submit(entered("In Review", "d1"));
    assert!(wait_until(|| !s.linear.comments().is_empty()).await);
    let before = watch_of(&s.h).await.unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;
    s.github.set_head(316, &"b".repeat(40));

    sink.submit(InboundEvent::GithubPrPushed {
        repo: REPO.into(),
        pr_number: 316,
        head_sha: "b".repeat(40),
        delivery_id: Some("p1".into()),
    });

    let mut after = before.clone();
    for _ in 0..500 {
        after = watch_of(&s.h).await.unwrap();
        if after.prs[0].head_sha != before.prs[0].head_sha {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(after.prs[0].head_sha, "b".repeat(40));
    assert!(after.created_at > before.created_at);
}

#[sqlx::test(migrations = "./migrations")]
async fn a_watched_issue_that_is_done_drops_the_watch_and_moves_nothing(pool: PgPool) {
    let s = setup(Harness::new(pool).with_fake(50, 0, None, false));
    s.github.set_ci(Ci::Pending);
    let (_d, sink) = s.h.start_with_reviews(s.linear.clone(), s.github.clone());
    sink.submit(entered("In Review", "d1"));
    assert!(wait_until(|| !s.linear.comments().is_empty()).await);

    set_issue_state(&s.linear, "Done");
    sink.submit(InboundEvent::LinearIssueStateChanged {
        change: IssueStateChange {
            issue_id: ISSUE_ID.into(),
            identifier: "NEX-1".into(),
            state_name: "Done".into(),
            state_type: "completed".into(),
        },
        delivery_id: Some("d2".into()),
    });
    s.github.set_ci(Ci::Green);

    let mut gone = false;
    for _ in 0..500 {
        if watch_of(&s.h).await.is_none() {
            gone = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(gone, "watch dropped");
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert_eq!(*s.linear.moves.lock().unwrap(), vec!["In Progress"]);
}

#[sqlx::test(migrations = "./migrations")]
async fn an_issue_moved_by_hand_while_watched_is_left_alone(pool: PgPool) {
    let s = setup(Harness::new(pool).with_fake(50, 0, None, false));
    s.github.set_ci(Ci::Pending);
    let (_d, sink) = s.h.start_with_reviews(s.linear.clone(), s.github.clone());
    sink.submit(entered("In Review", "d1"));
    assert!(wait_until(|| !s.linear.comments().is_empty()).await);

    set_issue_state(&s.linear, "Backlog");
    s.github.set_ci(Ci::Green);
    tokio::time::sleep(Duration::from_millis(600)).await;

    assert_eq!(*s.linear.moves.lock().unwrap(), vec!["In Progress"]);
    assert!(watch_of(&s.h).await.is_some(), "the watch stays");
}

#[sqlx::test(migrations = "./migrations")]
async fn the_poll_catches_a_lost_webhook(pool: PgPool) {
    let s = setup(Harness::new(pool).with_fake(50, 0, None, false));
    s.github.set_ci(Ci::Pending);
    let (_d, sink) = s.h.start_with_reviews(s.linear.clone(), s.github.clone());
    sink.submit(entered("In Review", "d1"));
    assert!(wait_until(|| !s.linear.comments().is_empty()).await);

    s.github.set_ci(Ci::Green);

    assert!(
        wait_until(|| s.linear.moves.lock().unwrap().len() == 2).await,
        "promoted by the poll"
    );
    assert_eq!(s.linear.moves.lock().unwrap()[1], "In Review");
}

#[sqlx::test(migrations = "./migrations")]
async fn a_failure_on_a_watched_issue_sends_it_to_todo_and_stops_watching(pool: PgPool) {
    let s = setup(Harness::new(pool).with_fake(50, 0, None, false));
    s.github.set_ci(Ci::Pending);
    let (_d, sink) = s.h.start_with_reviews(s.linear.clone(), s.github.clone());
    sink.submit(entered("In Review", "d1"));
    assert!(wait_until(|| !s.linear.comments().is_empty()).await);

    s.github.set_ci(Ci::Failed);
    sink.submit(checks_done("c1", &s.sha));

    assert!(wait_until(|| !s.linear.moved.lock().unwrap().is_empty()).await);
    assert!(wait_until(|| s.linear.comments().len() == 2).await);
    assert!(watch_of(&s.h).await.is_none());
}

#[sqlx::test(migrations = "./migrations")]
async fn the_watch_times_out_with_a_comment_and_the_issue_stays_in_progress(pool: PgPool) {
    let mut h = Harness::new(pool).with_fake(50, 0, None, false);
    h.config.review.ci_max_wait = Duration::from_millis(400);
    let s = setup(h);
    s.github.set_ci(Ci::Pending);
    let (_d, sink) = s.h.start_with_reviews(s.linear.clone(), s.github.clone());

    sink.submit(entered("In Review", "d1"));

    assert!(wait_until(|| s.linear.comments().len() == 2).await);
    assert!(s.linear.comments()[1].contains("stops watching"));
    assert!(watch_of(&s.h).await.is_none());
    assert_eq!(*s.linear.moves.lock().unwrap(), vec!["In Progress"]);
}

#[sqlx::test(migrations = "./migrations")]
async fn a_redelivered_checks_webhook_makes_no_second_move(pool: PgPool) {
    let s = setup(Harness::new(pool).with_fake(50, 0, None, false));
    s.github.set_ci(Ci::Pending);
    let (_d, sink) = s.h.start_with_reviews(s.linear.clone(), s.github.clone());
    sink.submit(entered("In Review", "d1"));
    assert!(wait_until(|| !s.linear.comments().is_empty()).await);

    s.github.set_ci(Ci::Green);
    sink.submit(checks_done("c1", &s.sha));
    sink.submit(checks_done("c1", &s.sha));
    assert!(wait_until(|| s.linear.moves.lock().unwrap().len() == 2).await);
    tokio::time::sleep(Duration::from_millis(500)).await;

    assert_eq!(
        *s.linear.moves.lock().unwrap(),
        vec!["In Progress", "In Review"]
    );
    assert_eq!(s.linear.comments().len(), 1);
}

#[sqlx::test(migrations = "./migrations")]
async fn a_manual_review_skips_the_gate_even_with_ci_red(pool: PgPool) {
    let s = setup(Harness::new(pool).with_fake(50, 0, None, false));
    s.github.set_ci(Ci::Failed);
    s.h.fake.set_turn(50, Some(&verdict_cmd("approve", &s.sha)));
    let (_d, sink) = s.h.start_with_reviews(s.linear.clone(), s.github.clone());

    sink.submit(InboundEvent::ReviewCommand {
        command: ReviewCommand {
            repo: REPO.into(),
            pr_number: 316,
            comment_id: Some(42),
            login: "lassemand".into(),
        },
        delivery_id: Some("gh-1".into()),
    });

    assert!(wait_for_closed(&s.h).await);
    assert_eq!(review_prompts(&s.h).len(), 1);
    assert!(review_prompts(&s.h)[0].contains("Trigger: manual"));
    assert!(s.linear.moves.lock().unwrap().is_empty());
}

fn set_author(github: &FakeGithub, number: u64, login: &str) {
    github.prs.lock().unwrap().get_mut(&number).unwrap().author = login.into();
}

fn manual(login: &str, delivery: &str) -> InboundEvent {
    InboundEvent::ReviewCommand {
        command: ReviewCommand {
            repo: REPO.into(),
            pr_number: 316,
            comment_id: Some(9),
            login: login.into(),
        },
        delivery_id: Some(delivery.into()),
    }
}

#[sqlx::test(migrations = "./migrations")]
async fn a_pull_request_by_a_watched_user_is_reviewed(pool: PgPool) {
    let s = setup(Harness::new(pool).with_fake(50, 0, None, false));
    set_author(&s.github, 316, "lassemand");
    s.h.fake.set_turn(50, Some(&verdict_cmd("approve", &s.sha)));
    let (_d, sink) = s.h.start_with_reviews(s.linear.clone(), s.github.clone());

    sink.submit(entered("In Review", "d1"));

    assert!(wait_for_closed(&s.h).await);
    assert_eq!(review_prompts(&s.h).len(), 1);
}

#[sqlx::test(migrations = "./migrations")]
async fn a_pull_request_by_the_author_app_is_reviewed(pool: PgPool) {
    let s = setup(Harness::new(pool).with_fake(50, 0, None, false));
    set_author(&s.github, 316, "nexus-mcp[bot]");
    s.h.fake.set_turn(50, Some(&verdict_cmd("approve", &s.sha)));
    let (_d, sink) = s.h.start_with_reviews(s.linear.clone(), s.github.clone());

    sink.submit(entered("In Review", "d1"));

    assert!(wait_for_closed(&s.h).await);
    assert_eq!(review_prompts(&s.h).len(), 1);
}

#[sqlx::test(migrations = "./migrations")]
async fn the_author_match_ignores_case(pool: PgPool) {
    let s = setup(Harness::new(pool).with_fake(50, 0, None, false));
    set_author(&s.github, 316, "LassemAND");
    s.h.fake.set_turn(50, Some(&verdict_cmd("approve", &s.sha)));
    let (_d, sink) = s.h.start_with_reviews(s.linear.clone(), s.github.clone());

    sink.submit(entered("In Review", "d1"));

    assert!(wait_for_closed(&s.h).await);
    assert_eq!(review_prompts(&s.h).len(), 1);
}

#[sqlx::test(migrations = "./migrations")]
async fn an_outside_author_gets_no_review_and_no_side_effect_on_an_automatic_trigger(pool: PgPool) {
    let s = setup(Harness::new(pool).with_fake(50, 0, None, false));
    set_author(&s.github, 316, "stranger");
    s.github.set_ci(Ci::Pending);
    let (_d, sink) = s.h.start_with_reviews(s.linear.clone(), s.github.clone());

    sink.submit(entered("In Review", "d1"));

    assert!(wait_until(|| !s.linear.comments().is_empty()).await);
    assert_eq!(
        s.linear.comments(),
        vec![format!(
            "Review not started: no pull request by an allowed author ({REPO} PR 316 by stranger)"
        )]
    );
    assert!(s.h.group(&review_key()).await.is_none());
    assert!(store::run(&s.h.pool, ISSUE_ID).await.unwrap().is_none());
    assert!(
        s.linear.moves.lock().unwrap().is_empty(),
        "the issue stays in review: no state move"
    );
    assert!(store::watch(&s.h.pool, ISSUE_ID).await.unwrap().is_none());
    assert_eq!(
        s.github.check_calls.load(Ordering::SeqCst),
        0,
        "the CI gate never ran"
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn an_outside_author_gets_no_review_on_a_review_command_either(pool: PgPool) {
    let s = setup(Harness::new(pool).with_fake(50, 0, None, false));
    set_author(&s.github, 316, "stranger");
    let (_d, sink) = s.h.start_with_reviews(s.linear.clone(), s.github.clone());

    sink.submit(manual("lassemand", "c1"));

    assert!(wait_until(|| !s.linear.comments().is_empty()).await);
    assert!(s.linear.comments()[0].starts_with("Review not started: no pull request by an allowed author"));
    assert!(s.h.group(&review_key()).await.is_none());
    assert!(s.github.reactions.lock().unwrap().is_empty());
}

#[sqlx::test(migrations = "./migrations")]
async fn a_review_command_from_an_outsider_on_an_allowed_pr_still_reaches_the_trigger_check(pool: PgPool) {
    let s = setup(Harness::new(pool).with_fake(50, 0, None, false));
    set_author(&s.github, 316, "lassemand");
    s.h.fake.set_turn(50, Some(&verdict_cmd("approve", &s.sha)));
    let (_d, sink) = s.h.start_with_reviews(s.linear.clone(), s.github.clone());

    sink.submit(manual("lassemand", "c1"));

    assert!(wait_for_closed(&s.h).await);
    assert_eq!(review_prompts(&s.h).len(), 1);
}

#[sqlx::test(migrations = "./migrations")]
async fn mixed_pull_requests_review_only_the_allowed_ones(pool: PgPool) {
    let s = setup(Harness::new(pool).with_fake(50, 0, None, false));
    {
        let mut prs = s.github.prs.lock().unwrap();
        let other = prs.get_mut(&300).unwrap();
        other.open = true;
        other.author = "stranger".into();
    }
    s.h.fake.set_turn(50, Some(&verdict_cmd("approve", &s.sha)));
    let (_d, sink) = s.h.start_with_reviews(s.linear.clone(), s.github.clone());

    sink.submit(entered("In Review", "d1"));

    assert!(wait_for_closed(&s.h).await);
    let prompts = review_prompts(&s.h);
    assert_eq!(prompts.len(), 1);
    assert!(prompts[0].contains("number=316"));
    assert!(!prompts[0].contains("number=300"));
    assert!(s
        .linear
        .comments()
        .iter()
        .any(|c| c.contains(&format!("{REPO} PR 300 by stranger")) && c.contains("not allowed")));
}

#[sqlx::test(migrations = "./migrations")]
async fn an_unknown_author_login_is_not_allowed(pool: PgPool) {
    let s = setup(Harness::new(pool).with_fake(50, 0, None, false));
    set_author(&s.github, 316, "");
    let (_d, sink) = s.h.start_with_reviews(s.linear.clone(), s.github.clone());

    sink.submit(entered("In Review", "d1"));

    assert!(wait_until(|| !s.linear.comments().is_empty()).await);
    assert!(s.linear.comments()[0].contains("by an unknown author"));
    assert!(s.h.group(&review_key()).await.is_none());
}

#[sqlx::test(migrations = "./migrations")]
async fn an_identity_that_is_not_ready_starts_no_session_and_says_why(pool: PgPool) {
    let mut h = Harness::new(pool).with_fake(50, 0, None, false);
    let dir = h.state.join("unready-identity");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("not-ready"), "the reviewer key is not usable\n").unwrap();
    let launch = conductor::dispatcher::ReviewLaunch {
        launcher: "conductor-review-session".into(),
        identity_dir: dir,
    };
    h.config.review_launch = Some(launch.clone());
    h.config.review.launch = Some(launch);
    let s = setup(h);
    let (_d, sink) = s.h.start_with_reviews(s.linear.clone(), s.github.clone());

    sink.submit(entered("In Review", "d1"));

    assert!(wait_until(|| !s.linear.comments().is_empty()).await);
    assert_eq!(
        s.linear.comments(),
        vec![
            "Review not started: the reviewer identity is not ready (the reviewer key is not usable)"
                .to_string()
        ]
    );
    assert!(s.h.group(&review_key()).await.is_none());
    assert!(store::run(&s.h.pool, ISSUE_ID).await.unwrap().is_none());
    assert!(s.linear.moves.lock().unwrap().is_empty());
    assert!(s.linear.moved.lock().unwrap().is_empty());
}

#[sqlx::test(migrations = "./migrations")]
async fn no_review_identity_directory_at_all_also_starts_no_session(pool: PgPool) {
    let mut h = Harness::new(pool).with_fake(50, 0, None, false);
    h.config.review_launch = None;
    h.config.review.launch = None;
    let s = setup(h);
    let (_d, sink) = s.h.start_with_reviews(s.linear.clone(), s.github.clone());

    sink.submit(entered("In Review", "d1"));

    assert!(wait_until(|| !s.linear.comments().is_empty()).await);
    assert!(s.linear.comments()[0]
        .starts_with("Review not started: the reviewer identity is not ready ("));
    assert!(s.h.group(&review_key()).await.is_none());
}

#[sqlx::test(migrations = "./migrations")]
async fn a_review_checkout_is_a_standalone_clone_the_reviewer_uid_can_write(pool: PgPool) {
    use std::os::unix::fs::PermissionsExt;

    let h = Harness::new(pool);
    let sha = git(&h.repo, &["rev-parse", "HEAD"]).trim().to_string();
    let path = ensure_detached_worktree(&h.repo, &h.worktrees, "review-nex-9", &sha)
        .await
        .unwrap();

    assert!(path.join(".git").is_dir(), ".git is a directory, not a link back to the author's repository");
    assert!(!path.join(".git/commondir").exists());
    assert!(!path.join(".git/objects/info/alternates").exists());
    let repo_git = std::fs::canonicalize(h.repo.join(".git")).unwrap();
    let clone_git = std::fs::canonicalize(path.join(".git")).unwrap();
    assert_ne!(repo_git, clone_git);
    assert_eq!(
        git(&path, &["remote", "get-url", "origin"]).trim(),
        h.origin.to_str().unwrap(),
        "origin is the real remote, so the reviewer fetches from GitHub"
    );
    for inside in [".", ".git", ".git/objects", "README.md"] {
        let mode = std::fs::metadata(path.join(inside)).unwrap().permissions().mode();
        assert_eq!(mode & 0o006, 0o006, "{inside} is readable and writable by other users: {mode:o}");
    }
    assert!(
        !h.repo.join(".git/worktrees").exists(),
        "no worktree is registered in the author's repository"
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn closing_a_review_removes_its_checkout_without_running_git_in_it(pool: PgPool) {
    let s = setup(Harness::new(pool).with_fake(50, 0, None, false));
    s.h.fake.set_turn(50, Some(&verdict_cmd("approve", &s.sha)));
    let (_d, sink) = s.h.start_with_reviews(s.linear.clone(), s.github.clone());
    sink.submit(entered("In Review", "d1"));
    assert!(wait_for_closed(&s.h).await);
    assert!(!s.h.worktrees.join("review-nex-1").exists());
}

#[test]
fn a_verdict_that_is_a_link_or_too_large_is_never_read() {
    let dir = tempfile::tempdir().unwrap();
    let secret = dir.path().join("secret.json");
    std::fs::write(&secret, r#"{"verdict":"approve","prs":[],"summary":"leaked"}"#).unwrap();
    let link = dir.path().join("verdict.json");
    std::os::unix::fs::symlink(&secret, &link).unwrap();
    let err = conductor::review::read_verdict(&link, &[]).unwrap_err();
    assert!(err.contains("could not be read"), "{err}");

    let big = dir.path().join("big.json");
    std::fs::write(&big, vec![b' '; 300 * 1024]).unwrap();
    let err = conductor::review::read_verdict(&big, &[]).unwrap_err();
    assert!(err.contains("could not be read"), "{err}");

    let ok = dir.path().join("ok.json");
    std::fs::write(&ok, r#"{"verdict":"approve","prs":[],"summary":"fine"}"#).unwrap();
    assert!(conductor::review::read_verdict(&ok, &[]).is_ok());
}
