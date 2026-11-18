//! Automatic reviews of the pull requests behind a Linear issue.
//!
//! When an issue moves into the review state (`In Review`), or someone
//! comments `/review` on one of its pull requests, a fresh Claude session
//! running the `reviewer` agent reviews every open pull request attached to
//! the issue. It runs in its own set, `review-<identifier>`, in a worktree
//! detached at the pull request head, under its own concurrency limit.
//!
//! The reviewer writes `.review/verdict.json` in its worktree. When its turn
//! ends conductor reads the verdict and does the side effects itself:
//! `changes_requested` sends the issue back to Todo (where the author's set
//! picks it up again), `approve` leaves it in review for a human to merge.
//! After at most `CONDUCTOR_REVIEW_MAX_ITERATIONS` send-backs an issue
//! entering review gets no automatic review, only the `needs-human` label and
//! a Linear comment; `/review` still works.
//!
//! An automatic review starts only when every check run on the head of every
//! open pull request has passed ([`ci_gate`]); an issue that enters review
//! earlier is moved out again and watched until CI settles.
//!
//! Every review-related Linear write goes through [`ReviewLinear`], which in
//! production writes as the reviewer's own Linear app.

pub mod ci_gate;
pub mod cli;
pub mod store;

use std::collections::HashSet;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use chrono::Utc;
use serde::{Deserialize, Serialize};
use tokio::sync::Notify;

use crate::closing::{close_group, CloseMode, CloseOutcome};
use crate::github::ReviewCommand;
use crate::github_api::GithubApiError;
use crate::linear::IssueStateChange;
use crate::linear_api::LinearApiError;
use crate::registry::{Group, GroupKey, GroupState, Registry, RunResult};
use crate::tmux::SessionRuntime;
use crate::InboundEvent;

use store::ReviewRun;

/// Label put on a pull request whose issue reached the send-back limit.
pub const NEEDS_HUMAN_LABEL: &str = "needs-human";

/// Where in its worktree the reviewer writes its verdict.
pub const VERDICT_FILE: &str = ".review/verdict.json";

/// Prefix of a Linear issue key in branch names and links.
const ISSUE_KEY_PREFIX: &str = "nex-";

/// How long a review set may sit in `Starting` or `Delivering`, which it
/// enters only with a review slot, before the review is failed.
const STUCK_GRACE: Duration = Duration::from_secs(10 * 60);

/// Longest pause between retries of a Linear write.
const MAX_RETRY_DELAY: Duration = Duration::from_secs(60);

/// How often closing a review set is tried while its state keeps changing,
/// as it does right after an interrupt.
const CLOSE_ATTEMPTS: u32 = 10;

/// How often a pull request's current head is re-read before giving up.
const HEAD_LOOKUP_ATTEMPTS: u32 = 3;

/// Review settings, parsed by `clap` from flags or environment.
#[derive(Debug, Clone, clap::Args)]
pub struct ReviewArgs {
    /// Turns every automatic and manual review trigger on or off.
    #[arg(long, env = "CONDUCTOR_REVIEW_ENABLED", default_value_t = true, action = clap::ArgAction::Set)]
    pub review_enabled: bool,

    /// Linear state whose entry starts an automatic review.
    #[arg(long, env = "CONDUCTOR_REVIEW_STATE", default_value = "In Review")]
    pub review_state: String,

    /// Most send-backs per issue before automatic reviews stop; 0 means manual only.
    #[arg(long, env = "CONDUCTOR_REVIEW_MAX_ITERATIONS", default_value_t = 2)]
    pub review_max_iterations: u32,

    /// Concurrent review sessions, counted apart from `CONDUCTOR_MAX_SESSIONS`.
    #[arg(long, env = "CONDUCTOR_MAX_REVIEW_SESSIONS", default_value_t = 1, value_parser = clap::value_parser!(u16).range(1..))]
    pub max_review_sessions: u16,

    /// How long a review turn may run: `45m`, `2700s`, `1h`, or plain minutes.
    #[arg(long, env = "CONDUCTOR_REVIEW_TIMEOUT", default_value = "45m", value_parser = parse_duration)]
    pub review_timeout: Duration,

    /// GitHub login of the reviewer app, whose comments never reach a session.
    #[arg(long, env = "CONDUCTOR_REVIEW_BOT_LOGIN")]
    pub review_bot_login: Option<String>,

    /// GitHub login of the author app (`<slug>[bot]`), whose pull requests are
    /// reviewed in addition to those of `GITHUB_WEBHOOK_USERS`. The entrypoint
    /// reads it from the app (`GET /app`); set it to override or to supply it
    /// when that read failed. It is for reviews only: it is never a watched
    /// user, so the agent's own comments never come back as events.
    #[arg(long, env = "CONDUCTOR_AUTHOR_BOT_LOGIN")]
    pub author_bot_login: Option<String>,

    /// `owner/name` of the repository reviews cover; defaults to the `origin`
    /// remote of the repository the worktrees come from.
    #[arg(long, env = "CONDUCTOR_REVIEW_REPO")]
    pub review_repo: Option<String>,

    /// How often watched issues are re-checked, in case a webhook is lost.
    #[arg(long, env = "CONDUCTOR_CI_POLL_INTERVAL", default_value = "60s", value_parser = parse_duration)]
    pub ci_poll_interval: Duration,

    /// How long an issue is watched for green CI before conductor gives up.
    #[arg(long, env = "CONDUCTOR_CI_MAX_WAIT", default_value = "2h", value_parser = parse_duration)]
    pub ci_max_wait: Duration,
}

impl ReviewArgs {
    /// Resolves the arguments; the agent and model come from the session
    /// settings, which `conductor sessions` shares.
    pub fn into_config(self, agent: String, model: Option<String>) -> ReviewConfig {
        let author_bot_login = self.author_bot_login.filter(|l| !l.trim().is_empty());
        let mut allowed_authors = crate::github::watched_users_from_env();
        allowed_authors.extend(author_bot_login.clone());
        ReviewConfig {
            allowed_authors,
            author_bot_login,
            launch: None,
            enabled: self.review_enabled,
            state: self.review_state,
            max_iterations: self.review_max_iterations,
            model,
            agent,
            max_sessions: usize::from(self.max_review_sessions),
            timeout: self.review_timeout,
            bot_login: self.review_bot_login.filter(|l| !l.is_empty()),
            repo: self.review_repo.filter(|r| !r.is_empty()),
            ci_poll_interval: self.ci_poll_interval,
            ci_max_wait: self.ci_max_wait,
            linear_write_budget: Duration::from_secs(10 * 60),
            linear_retry: Duration::from_secs(2),
        }
    }
}

/// Parses `45m`, `2700s`, `1h` or a plain number of minutes.
pub fn parse_duration(value: &str) -> Result<Duration, String> {
    let value = value.trim();
    let (digits, unit) = match value.char_indices().find(|(_, c)| !c.is_ascii_digit()) {
        Some((i, _)) => value.split_at(i),
        None => (value, "m"),
    };
    let n: u64 = digits
        .parse()
        .map_err(|_| format!("invalid duration {value:?}: expected e.g. 45m, 2700s or 1h"))?;
    let secs = match unit {
        "s" => n,
        "m" => n.saturating_mul(60),
        "h" => n.saturating_mul(3600),
        _ => {
            return Err(format!(
                "invalid duration {value:?}: unit must be s, m or h"
            ))
        }
    };
    if secs == 0 {
        return Err(format!("invalid duration {value:?}: must be positive"));
    }
    Ok(Duration::from_secs(secs))
}

/// Resolved review settings.
#[derive(Debug, Clone)]
pub struct ReviewConfig {
    /// `false` turns every trigger off.
    pub enabled: bool,
    /// GitHub logins whose pull requests are reviewed: `GITHUB_WEBHOOK_USERS`
    /// plus the author app's bot login. Compared case-insensitively. A pull
    /// request by anyone else never reaches a review session.
    pub allowed_authors: Vec<String>,
    /// The author app's bot login, when known.
    pub author_bot_login: Option<String>,
    /// How review sessions are started; a review starts only when its
    /// identity is ready.
    pub launch: Option<crate::dispatcher::ReviewLaunch>,
    /// Linear state whose entry starts an automatic review.
    pub state: String,
    /// Most send-backs before automatic reviews stop.
    pub max_iterations: u32,
    /// Model passed as `--model` to review sessions.
    pub model: Option<String>,
    /// Agent review sessions run.
    pub agent: String,
    /// Concurrent review sessions.
    pub max_sessions: usize,
    /// How long a review turn may run before it is interrupted and failed.
    pub timeout: Duration,
    /// The reviewer app's GitHub login.
    pub bot_login: Option<String>,
    /// `owner/name` of the repository the worktrees come from. Only its pull
    /// requests are reviewed; `None` reviews whatever the issue lists.
    pub repo: Option<String>,
    /// How often watched issues are re-checked.
    pub ci_poll_interval: Duration,
    /// How long an issue is watched for green CI.
    pub ci_max_wait: Duration,
    /// How long a Linear write is retried before the review counts as failed.
    pub linear_write_budget: Duration,
    /// First pause between Linear write retries; doubles up to a minute.
    pub linear_retry: Duration,
}

impl Default for ReviewConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            allowed_authors: vec!["lassemand".into()],
            author_bot_login: None,
            launch: None,
            state: "In Review".into(),
            max_iterations: 2,
            model: Some("opus".into()),
            agent: "reviewer".into(),
            max_sessions: 1,
            timeout: Duration::from_secs(45 * 60),
            bot_login: None,
            repo: None,
            ci_poll_interval: Duration::from_secs(60),
            ci_max_wait: Duration::from_secs(2 * 3600),
            linear_write_budget: Duration::from_secs(10 * 60),
            linear_retry: Duration::from_secs(2),
        }
    }
}

/// What started a review.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Trigger {
    /// The issue entered the review state.
    Automatic,
    /// A `/review` comment.
    Manual,
}

impl Trigger {
    /// The label used in metrics, the database and the prompt.
    pub fn as_str(self) -> &'static str {
        match self {
            Trigger::Automatic => "automatic",
            Trigger::Manual => "manual",
        }
    }

    /// Inverse of [`Trigger::as_str`].
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "automatic" => Some(Trigger::Automatic),
            "manual" => Some(Trigger::Manual),
            _ => None,
        }
    }
}

/// A pull request as the prompt named it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PrUnderReview {
    /// `owner/name`.
    pub repo: String,
    pub number: u64,
    pub url: String,
    pub head_sha: String,
    pub head_ref: String,
}

/// `(owner/name, number)` of pull requests.
type PrRefs = Vec<(String, u64)>;

/// A review to paste into a review session.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ReviewRequest {
    pub identifier: String,
    pub issue_url: String,
    pub prs: Vec<PrUnderReview>,
    /// 1 for the issue's first review.
    pub review_number: i32,
    pub send_backs: i32,
    pub limit: u32,
    pub trigger: Trigger,
    /// Absolute path the verdict must be written to.
    pub verdict_path: String,
}

impl ReviewRequest {
    /// The prompt: plain text, one labelled line per fact.
    pub fn prompt(&self) -> String {
        let mut lines = vec![
            "Review request from conductor.".to_string(),
            format!("Linear issue: {}", self.identifier),
            format!("Linear URL: {}", self.issue_url),
        ];
        for pr in &self.prs {
            lines.push(format!(
                "PR: number={} url={} head_sha={}",
                pr.number, pr.url, pr.head_sha
            ));
        }
        lines.push(format!("Review number: {}", self.review_number));
        lines.push(format!("Send-backs so far: {}", self.send_backs));
        lines.push(format!("Send-back limit: {}", self.limit));
        lines.push(format!("Trigger: {}", self.trigger.as_str()));
        lines.push(format!("Verdict file: {}", self.verdict_path));
        lines.join("\n")
    }
}

/// A Linear issue as the review orchestration sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewIssue {
    /// Linear's UUID.
    pub id: String,
    pub identifier: String,
    pub url: String,
    pub state_name: String,
    pub team_id: String,
    /// URLs of every attachment; the GitHub pull requests are picked out.
    pub attachment_urls: Vec<String>,
}

/// A pull request as GitHub reports it now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PullRequest {
    pub repo: String,
    pub number: u64,
    pub url: String,
    /// Open and not merged.
    pub open: bool,
    pub head_sha: String,
    pub head_ref: String,
    /// Repository the head branch lives in, when GitHub says.
    pub head_repo: Option<String>,
    /// Repository the pull request targets, when GitHub says.
    pub base_repo: Option<String>,
    pub body: String,
    /// GitHub login of the pull request's author (`user.login`); empty when
    /// GitHub did not say, which no allow-list matches.
    pub author: String,
}

/// The Linear calls reviews make. Reads may use any identity; every write is
/// made as the reviewer.
#[async_trait::async_trait]
pub trait ReviewLinear: Send + Sync {
    /// An issue by UUID or identifier; `None` when it does not exist.
    async fn issue(&self, id: &str) -> Result<Option<ReviewIssue>, LinearApiError>;
    /// Posts a comment on the issue.
    async fn comment(&self, issue_id: &str, body: &str) -> Result<(), LinearApiError>;
    /// Moves the issue to Todo.
    async fn move_to_todo(&self, issue: &ReviewIssue) -> Result<(), LinearApiError>;
    /// Moves the issue to the team's workflow state with this name.
    async fn move_to_state(&self, issue: &ReviewIssue, state: &str) -> Result<(), LinearApiError>;
}

/// The GitHub calls reviews make.
#[async_trait::async_trait]
pub trait ReviewGithub: Send + Sync {
    /// A pull request's current state.
    async fn pull_request(&self, repo: &str, number: u64) -> Result<PullRequest, GithubApiError>;
    /// Adds a label to a pull request.
    async fn add_label(&self, repo: &str, number: u64, label: &str) -> Result<(), GithubApiError>;
    /// Removes a label; an absent label is not an error.
    async fn remove_label(
        &self,
        repo: &str,
        number: u64,
        label: &str,
    ) -> Result<(), GithubApiError>;
    /// Reacts to an issue comment.
    async fn react(&self, repo: &str, comment_id: u64, content: &str)
        -> Result<(), GithubApiError>;
    /// Comments on a pull request.
    async fn comment(&self, repo: &str, number: u64, body: &str) -> Result<(), GithubApiError>;
    /// Every check run reported for a commit.
    async fn check_runs(
        &self,
        repo: &str,
        sha: &str,
    ) -> Result<Vec<ci_gate::CheckRun>, GithubApiError>;
}

/// The external clients a [`Reviews`] needs.
pub struct ReviewDeps {
    pub config: ReviewConfig,
    pub linear: Arc<dyn ReviewLinear>,
    pub github: Arc<dyn ReviewGithub>,
}

/// `owner/name` of a GitHub remote URL (`https://…`, with or without
/// credentials or `.git`, or `git@github.com:owner/name.git`).
pub fn github_repo_from_remote(url: &str) -> Option<String> {
    let url = url.trim();
    let rest = url
        .split_once("github.com/")
        .or_else(|| url.split_once("github.com:"))?
        .1;
    let rest = rest.trim_end_matches('/');
    let rest = rest.strip_suffix(".git").unwrap_or(rest);
    let (owner, name) = rest.split_once('/')?;
    (!owner.is_empty() && !name.is_empty() && !name.contains('/'))
        .then(|| format!("{owner}/{name}"))
}

/// `owner/name` and number of a GitHub pull request URL.
pub fn parse_pr_url(url: &str) -> Option<(String, u64)> {
    let rest = url
        .strip_prefix("https://github.com/")
        .or_else(|| url.strip_prefix("http://github.com/"))?;
    let mut parts = rest.split('/');
    let owner = parts.next().filter(|s| !s.is_empty())?;
    let name = parts.next().filter(|s| !s.is_empty())?;
    if parts.next()? != "pull" {
        return None;
    }
    let number: String = parts
        .next()?
        .chars()
        .take_while(char::is_ascii_digit)
        .collect();
    Some((format!("{owner}/{name}"), number.parse().ok()?))
}

/// The first `NEX-<n>` in `text` that starts a word, uppercased.
fn find_issue_key(text: &str) -> Option<String> {
    let lower = text.to_ascii_lowercase();
    let bytes = lower.as_bytes();
    let mut from = 0;
    while let Some(i) = lower[from..].find(ISSUE_KEY_PREFIX) {
        let start = from + i;
        let at_word_start = start == 0 || !bytes[start - 1].is_ascii_alphanumeric();
        let digits: String = lower[start + ISSUE_KEY_PREFIX.len()..]
            .chars()
            .take_while(char::is_ascii_digit)
            .collect();
        if at_word_start && !digits.is_empty() {
            return Some(format!("{}{digits}", ISSUE_KEY_PREFIX.to_ascii_uppercase()));
        }
        from = start + ISSUE_KEY_PREFIX.len();
    }
    None
}

/// The Linear issue a pull request belongs to: the `NEX-<n>` in its branch
/// name, else the issue in a Linear link in its body.
pub fn issue_key_from_pr(head_ref: &str, body: &str) -> Option<String> {
    if let Some(key) = find_issue_key(head_ref) {
        return Some(key);
    }
    body.match_indices("linear.app/").find_map(|(i, _)| {
        let url: String = body[i..]
            .chars()
            .take_while(|c| !c.is_whitespace() && !matches!(c, ')' | ']' | '>' | '"'))
            .collect();
        let after = url.split_once("/issue/")?.1;
        find_issue_key(after.split('/').next()?)
    })
}

/// A reviewer's verdict file.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct Verdict {
    pub verdict: VerdictKind,
    pub prs: Vec<VerdictPr>,
    #[serde(default)]
    pub summary: String,
}

/// What the reviewer decided.
#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum VerdictKind {
    Approve,
    ChangesRequested,
}

impl VerdictKind {
    fn as_str(self) -> &'static str {
        match self {
            VerdictKind::Approve => "approve",
            VerdictKind::ChangesRequested => "changes_requested",
        }
    }
}

/// One pull request in a verdict.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct VerdictPr {
    pub number: u64,
    pub head_sha: String,
    #[serde(default)]
    pub review_id: Option<u64>,
    #[serde(default)]
    pub blocking: Option<u32>,
    #[serde(default)]
    pub non_blocking: Option<u32>,
}

/// Reads and validates the verdict against the pull requests the prompt named.
/// The `Err` is the reason the review failed, phrased for a Linear comment.
pub fn read_verdict(path: &Path, prs: &[PrUnderReview]) -> Result<Verdict, String> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err("the reviewer wrote no verdict file".into())
        }
        Err(e) => return Err(format!("the verdict file could not be read ({e})")),
    };
    let verdict: Verdict =
        serde_json::from_str(&text).map_err(|e| format!("the verdict file is not valid ({e})"))?;
    for pr in prs {
        let Some(found) = verdict.prs.iter().find(|v| v.number == pr.number) else {
            return Err(format!("the verdict does not cover PR #{}", pr.number));
        };
        if found.head_sha != pr.head_sha {
            return Err(format!(
                "the verdict's head SHA for PR #{} does not match the one under review",
                pr.number
            ));
        }
    }
    Ok(verdict)
}

/// Links to the reviews a verdict names, one line per pull request.
fn review_links(verdict: &Verdict, prs: &[PrUnderReview]) -> String {
    prs.iter()
        .map(|pr| {
            let entry = verdict.prs.iter().find(|v| v.number == pr.number);
            let link = match entry.and_then(|v| v.review_id) {
                Some(id) => format!("{}#pullrequestreview-{id}", pr.url),
                None => pr.url.clone(),
            };
            let counts = match entry {
                Some(VerdictPr {
                    blocking: Some(b),
                    non_blocking: Some(n),
                    ..
                }) => format!(" ({b} blocking, {n} non-blocking)"),
                _ => String::new(),
            };
            format!("- PR #{}: {link}{counts}", pr.number)
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// The Linear comment for a send-back. Starts with `Review send-back k of N`,
/// which the author agent matches on.
pub fn send_back_comment(k: i32, limit: u32, verdict: &Verdict, prs: &[PrUnderReview]) -> String {
    format!(
        "Review send-back {k} of {limit}\n\n{}\n\n{}",
        verdict.summary.trim(),
        review_links(verdict, prs)
    )
}

/// The Linear comment for an approval.
pub fn approval_comment(verdict: &Verdict, prs: &[PrUnderReview]) -> String {
    format!(
        "Review approved\n\n{}\n\n{}",
        verdict.summary.trim(),
        review_links(verdict, prs)
    )
}

/// The Linear comment for a failed review.
pub fn failure_comment(reason: &str) -> String {
    format!("Automatic review failed: {reason}. Comment `/review` on the PR to retry.")
}

/// The Linear comment when automatic reviews have stopped for an issue.
pub fn limit_comment(send_backs: i32) -> String {
    format!(
        "Automatic review limit reached ({send_backs} send-backs). \
         Comment `/review` on the PR to request another review."
    )
}

/// A pull request left out of a review because of its author.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkippedPr {
    pub repo: String,
    pub number: u64,
    pub login: String,
}

impl SkippedPr {
    fn describe(&self) -> String {
        let login = if self.login.is_empty() {
            "an unknown author"
        } else {
            &self.login
        };
        format!("{} PR {} by {}", self.repo, self.number, login)
    }
}

/// The Linear comment when every pull request is by an author that is not allowed.
pub fn no_allowed_author_comment(skipped: &[SkippedPr]) -> String {
    let list = skipped
        .iter()
        .map(SkippedPr::describe)
        .collect::<Vec<_>>()
        .join(", ");
    format!("Review not started: no pull request by an allowed author ({list})")
}

/// The Linear comment when some pull requests are by authors that are not allowed.
pub fn skipped_authors_comment(skipped: &[SkippedPr]) -> String {
    let list = skipped
        .iter()
        .map(SkippedPr::describe)
        .collect::<Vec<_>>()
        .join(", ");
    format!("Review skipped for pull requests by an author that is not allowed: {list}")
}

/// The Linear comment when the reviewer identity is not ready.
pub fn identity_not_ready_comment(reason: &str) -> String {
    format!("Review not started: the reviewer identity is not ready ({reason})")
}

/// The Linear comment when there is nothing to review.
pub const NO_PR_COMMENT: &str =
    "Automatic review skipped: no open pull request is attached to this issue.";

/// What a trigger led to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TriggerOutcome {
    /// A review was queued.
    Queued,
    /// A review for the issue is already queued or running.
    Duplicate,
    /// The send-back limit was reached; the notice was posted instead.
    LimitReached,
    /// The issue has no open pull request.
    NoPr,
    /// Every open pull request is by an author that is not allowed.
    NoAllowedAuthor,
    /// The reviewer identity is not ready; no session was started.
    IdentityNotReady,
    /// The issue could not be found or read.
    Unavailable,
    /// An automatic trigger for an issue that is no longer in the review state.
    NotInReview,
    /// The checks are not all finished (or could not be read): the issue was
    /// moved to In Progress and is watched.
    CiPending,
    /// A check failed: the issue was moved to Todo.
    CiFailed,
}

/// How a finished review is handled.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Outcome {
    Verdict(Verdict),
    Failed(String),
    /// Thrown away; with `requeue`, one new review is queued.
    Discarded {
        reason: String,
        requeue: bool,
    },
}

/// Removes its key from an in-flight set when dropped.
struct InFlight<'a> {
    set: &'a StdMutex<HashSet<String>>,
    key: String,
}

impl<'a> InFlight<'a> {
    fn claim(set: &'a StdMutex<HashSet<String>>, key: &str) -> Option<Self> {
        set.lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(key.to_string())
            .then(|| InFlight {
                set,
                key: key.to_string(),
            })
    }
}

impl Drop for InFlight<'_> {
    fn drop(&mut self) {
        self.set
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&self.key);
    }
}

/// Runs the review orchestration beside the dispatcher.
pub struct Reviews {
    config: ReviewConfig,
    linear: Arc<dyn ReviewLinear>,
    github: Arc<dyn ReviewGithub>,
    registry: Registry,
    runtime: Arc<dyn SessionRuntime>,
    repo_root: PathBuf,
    worktree_root: PathBuf,
    /// The dispatcher's scheduler wake-up, so a queued review starts at once.
    wake: Arc<Notify>,
    /// Issues whose trigger is being handled, so two at once become one.
    triggering: StdMutex<HashSet<String>>,
    /// Issues whose finished review is being handled.
    finishing: StdMutex<HashSet<String>>,
    /// Watched issues being evaluated, so an event and the poll never both act.
    watching: StdMutex<HashSet<String>>,
    /// Set while a pass runs, so passes never overlap.
    passing: AtomicBool,
}

impl Reviews {
    /// Builds the orchestration on the dispatcher's registry, runtime and wake.
    pub fn new(
        deps: ReviewDeps,
        registry: Registry,
        runtime: Arc<dyn SessionRuntime>,
        repo_root: PathBuf,
        worktree_root: PathBuf,
        wake: Arc<Notify>,
    ) -> Self {
        Self {
            config: deps.config,
            linear: deps.linear,
            github: deps.github,
            registry,
            runtime,
            repo_root,
            worktree_root,
            wake,
            triggering: StdMutex::new(HashSet::new()),
            finishing: StdMutex::new(HashSet::new()),
            watching: StdMutex::new(HashSet::new()),
            passing: AtomicBool::new(false),
        }
    }

    /// The review settings.
    pub fn config(&self) -> &ReviewConfig {
        &self.config
    }

    /// Handles an event the HTTP layer produced for reviews. Spawned by the
    /// dispatcher, so Linear and GitHub calls never hold up ingest.
    pub async fn handle(self: Arc<Self>, event: InboundEvent) {
        match event {
            InboundEvent::LinearIssueStateChanged {
                change,
                delivery_id,
            } => self.on_state_change(change, delivery_id).await,
            InboundEvent::ReviewCommand {
                command,
                delivery_id,
            } => self.on_command(command, delivery_id).await,
            InboundEvent::GithubChecksCompleted {
                repo,
                head_sha,
                delivery_id,
            } => {
                self.on_checks_completed(&repo, &head_sha, delivery_id)
                    .await
            }
            InboundEvent::GithubPrPushed {
                repo,
                pr_number,
                head_sha,
                delivery_id,
            } => {
                self.on_pr_pushed(&repo, pr_number, &head_sha, delivery_id)
                    .await
            }
            _ => {}
        }
    }

    /// Whether this delivery is new to the review path. Keyed apart from the
    /// dispatcher's own record of the same delivery, which handles it too.
    async fn first_delivery(&self, delivery_id: Option<&str>) -> bool {
        let key = delivery_id.map(|d| format!("review:{d}"));
        match self.registry.mark_delivery_seen(key.as_deref()).await {
            Ok(new) => {
                if !new {
                    tracing::info!(delivery_id = ?delivery_id, "duplicate delivery ignored by reviews");
                }
                new
            }
            Err(e) => {
                tracing::error!(error = %e, "cannot record the delivery; review trigger dropped");
                false
            }
        }
    }

    async fn on_state_change(&self, change: IssueStateChange, delivery_id: Option<String>) {
        if !self.first_delivery(delivery_id.as_deref()).await {
            return;
        }
        if matches!(change.state_type.as_str(), "completed" | "canceled") {
            self.drop_watch(&change.issue_id, "the issue was closed")
                .await;
        }
        if change.state_name.eq_ignore_ascii_case(&self.config.state) {
            let outcome = self
                .trigger(&change.issue_id, Trigger::Automatic, false, None)
                .await;
            tracing::info!(issue = %change.identifier, ?outcome, "automatic review trigger handled");
            return;
        }
        match store::cancel_run(self.registry.pool(), &change.issue_id, Utc::now()).await {
            Ok(true) => {
                tracing::info!(
                    issue = %change.identifier,
                    state = %change.state_name,
                    "issue left review while its review ran; cancelling it"
                );
                self.wake.notify_one();
            }
            Ok(false) => {}
            Err(e) => {
                tracing::error!(issue = %change.identifier, error = %e, "cannot cancel the review")
            }
        }
    }

    async fn on_command(&self, command: ReviewCommand, delivery_id: Option<String>) {
        if !self.first_delivery(delivery_id.as_deref()).await {
            return;
        }
        let pr = match self
            .github
            .pull_request(&command.repo, command.pr_number)
            .await
        {
            Ok(pr) => pr,
            Err(e) => {
                tracing::error!(repo = %command.repo, pr = command.pr_number, error = %e, "cannot read the pull request for /review");
                return;
            }
        };
        let Some(identifier) = issue_key_from_pr(&pr.head_ref, &pr.body) else {
            tracing::warn!(repo = %command.repo, pr = command.pr_number, "/review on a pull request with no Linear issue");
            let body =
                "No Linear issue found for this pull request (no `NEX-<n>` in the branch name \
                        and no Linear link in the description), so it cannot be reviewed.";
            if let Err(e) = self
                .github
                .comment(&command.repo, command.pr_number, body)
                .await
            {
                tracing::error!(error = %e, "cannot answer the /review comment");
            }
            return;
        };
        let outcome = self
            .trigger(
                &identifier,
                Trigger::Manual,
                true,
                Some((command.repo.clone(), command.pr_number)),
            )
            .await;
        tracing::info!(issue = %identifier, by = %command.login, ?outcome, "manual review trigger handled");
        if let (TriggerOutcome::Queued, Some(comment_id)) = (outcome, command.comment_id) {
            if let Err(e) = self.github.react(&command.repo, comment_id, "eyes").await {
                tracing::warn!(error = %e, "cannot react to the /review comment");
            }
        }
    }

    /// Queues a review for an issue, unless one is queued or running, the
    /// limit is reached (automatic only, unless `bypass_limit`), or there is
    /// nothing to review. `extra` adds a pull request the issue may not list.
    pub async fn trigger(
        &self,
        issue_ref: &str,
        trigger: Trigger,
        bypass_limit: bool,
        extra: Option<(String, u64)>,
    ) -> TriggerOutcome {
        let issue = match self.linear.issue(issue_ref).await {
            Ok(Some(issue)) => issue,
            Ok(None) => {
                tracing::warn!(issue = %issue_ref, "issue not found in Linear; no review");
                return TriggerOutcome::Unavailable;
            }
            Err(e) => {
                tracing::error!(issue = %issue_ref, error = %e, "cannot read the issue from Linear; no review");
                return TriggerOutcome::Unavailable;
            }
        };
        if trigger == Trigger::Automatic && !self.in_review_state(&issue) {
            tracing::info!(issue = %issue.identifier, state = %issue.state_name, "issue is no longer in review; no automatic review");
            return TriggerOutcome::NotInReview;
        }
        let Some(key) = GroupKey::review(&issue.identifier) else {
            tracing::error!(issue = %issue.identifier, "identifier does not make a valid set name; no review");
            return TriggerOutcome::Unavailable;
        };
        let Some(_claim) = InFlight::claim(&self.triggering, &issue.id) else {
            tracing::info!(issue = %issue.identifier, "a review trigger for this issue is already being handled; dropped");
            return TriggerOutcome::Duplicate;
        };
        let pool = self.registry.pool();
        match store::run(pool, &issue.id).await {
            Ok(Some(_)) => {
                tracing::info!(issue = %issue.identifier, "a review is already queued or running; trigger dropped");
                return TriggerOutcome::Duplicate;
            }
            Ok(None) => {}
            Err(e) => {
                tracing::error!(error = %e, "cannot read review runs; no review");
                return TriggerOutcome::Unavailable;
            }
        }
        let state = match store::state(pool, &issue.id).await {
            Ok(s) => s.unwrap_or_default(),
            Err(e) => {
                tracing::error!(error = %e, "cannot read review state; no review");
                return TriggerOutcome::Unavailable;
            }
        };

        let limit = self.config.max_iterations;
        if trigger == Trigger::Automatic
            && !bypass_limit
            && i64::from(state.send_backs) >= i64::from(limit)
        {
            self.limit_reached(&issue, &state).await;
            return TriggerOutcome::LimitReached;
        }

        let (prs, skipped) = match self.open_prs(&issue, extra.clone()).await {
            Ok(found) => found,
            Err(reason) => {
                if trigger == Trigger::Automatic {
                    tracing::warn!(issue = %issue.identifier, %reason, "pull requests unreadable; CI cannot be confirmed");
                    let (refs, _) = self.split_refs(&issue, extra.clone());
                    return self.bounce_unconfirmed(&issue, &refs).await;
                }
                self.post_or_log(&issue.id, &failure_comment(&reason)).await;
                record_metric(trigger, "failed");
                return TriggerOutcome::Unavailable;
            }
        };
        for pr in &skipped {
            tracing::warn!(issue = %issue.identifier, repo = %pr.repo, pr = pr.number, author = %pr.login, "pull request by an author that is not allowed; not reviewed");
            metrics::counter!("conductor_review_skipped_total", "reason" => "author_not_allowed")
                .increment(1);
        }
        if prs.is_empty() {
            if !skipped.is_empty() {
                self.post_or_log(&issue.id, &no_allowed_author_comment(&skipped))
                    .await;
                return TriggerOutcome::NoAllowedAuthor;
            }
            tracing::info!(issue = %issue.identifier, "no open pull request; no review");
            self.post_or_log(&issue.id, NO_PR_COMMENT).await;
            return TriggerOutcome::NoPr;
        }
        if let Err(reason) = self.identity_ready() {
            tracing::error!(issue = %issue.identifier, %reason, "the reviewer identity is not ready; no review");
            metrics::counter!("conductor_review_failures_total", "reason" => "identity_not_ready")
                .increment(1);
            self.post_or_log(&issue.id, &identity_not_ready_comment(&reason))
                .await;
            return TriggerOutcome::IdentityNotReady;
        }
        if !skipped.is_empty() {
            self.post_or_log(&issue.id, &skipped_authors_comment(&skipped))
                .await;
        }
        if trigger == Trigger::Automatic {
            if let Err(outcome) = self.require_green(&issue, &prs).await {
                return outcome;
            }
        }

        let slug = key.slug();
        if let Ok(Some(existing)) = self.registry.group_by_slug(&slug).await {
            if existing.key != key {
                tracing::error!(group = %slug, found = ?existing.key, "a set that is not a review set holds the review set's name; no review");
                self.post_or_log(
                    &issue.id,
                    &failure_comment(&format!(
                        "the set `{slug}` already exists and is not a review set"
                    )),
                )
                .await;
                return TriggerOutcome::Unavailable;
            }
            tracing::warn!(group = %slug, "a stale review set exists; closing it first");
            self.close_set(&key).await;
        }

        let prs = if trigger == Trigger::Automatic {
            match self.reconfirm_green(&issue, extra.clone()).await {
                Ok(fresh) => fresh,
                Err(outcome) => return outcome,
            }
        } else {
            prs
        };

        let verdict_path = self.worktree_root.join(&slug).join(VERDICT_FILE);
        let _ = std::fs::remove_file(&verdict_path);
        let request = ReviewRequest {
            identifier: issue.identifier.clone(),
            issue_url: issue.url.clone(),
            prs: prs.clone(),
            review_number: state.reviews_total + 1,
            send_backs: state.send_backs,
            limit,
            trigger,
            verdict_path: verdict_path.to_string_lossy().into_owned(),
        };
        let run = ReviewRun {
            issue_id: issue.id.clone(),
            identifier: issue.identifier.clone(),
            issue_url: issue.url.clone(),
            group_slug: slug.clone(),
            trigger,
            review_number: request.review_number,
            prs,
            created_at: Utc::now(),
            cancelled_at: None,
            extra: extra.clone(),
        };
        match self.queue(&key, &run, request).await {
            Ok(true) => {
                tracing::info!(issue = %issue.identifier, group = %slug, trigger = trigger.as_str(), "review queued");
                self.wake.notify_one();
                TriggerOutcome::Queued
            }
            Ok(false) => {
                tracing::info!(issue = %issue.identifier, "a review is already queued or running; trigger dropped");
                TriggerOutcome::Duplicate
            }
            Err(e) => {
                tracing::error!(issue = %issue.identifier, error = %e, "cannot queue the review");
                TriggerOutcome::Unavailable
            }
        }
    }

    /// Creates the review set and queues the prompt, atomically with the run.
    async fn queue(
        &self,
        key: &GroupKey,
        run: &ReviewRun,
        request: ReviewRequest,
    ) -> Result<bool, sqlx::Error> {
        let mut txn = self.registry.begin().await?;
        if !store::insert_run(txn.conn(), run).await? {
            return Ok(false);
        }
        txn.session_for_dispatch(key).await?;
        let head = run.prs.first().map(|p| p.head_sha.as_str()).unwrap_or("");
        store::set_worktree_ref(txn.conn(), &key.slug(), head).await?;
        txn.enqueue(key, &InboundEvent::ReviewRequested { request })
            .await?;
        txn.commit().await?;
        Ok(true)
    }

    /// The pull requests an issue lists (plus `extra`), split into those in
    /// the reviewed repository and those in another one.
    fn split_refs(&self, issue: &ReviewIssue, extra: Option<(String, u64)>) -> (PrRefs, PrRefs) {
        let mut refs: Vec<(String, u64)> = Vec::new();
        for (repo, number) in issue
            .attachment_urls
            .iter()
            .filter_map(|u| parse_pr_url(u))
            .chain(extra)
        {
            if !refs.iter().any(|(r, n)| r == &repo && *n == number) {
                refs.push((repo, number));
            }
        }
        refs.into_iter().partition(|(repo, _)| {
            self.config
                .repo
                .as_deref()
                .is_none_or(|ours| ours.eq_ignore_ascii_case(repo))
        })
    }

    /// Whether review sessions can start: the reviewer's identity is prepared.
    /// `Err` carries the reason, which goes into the Linear comment.
    fn identity_ready(&self) -> Result<(), String> {
        match &self.config.launch {
            Some(launch) => launch.readiness(),
            None => Err("no review identity directory is configured".into()),
        }
    }

    /// Whether a pull request by `login` may be reviewed.
    fn author_allowed(&self, login: &str) -> bool {
        !login.is_empty()
            && self
                .config
                .allowed_authors
                .iter()
                .any(|a| a.eq_ignore_ascii_case(login))
    }

    /// The open ones of `refs`, read from GitHub now, split into those by an
    /// allowed author, with their current heads, and those that are not.
    async fn read_pr_set(
        &self,
        refs: &[(String, u64)],
    ) -> Result<(Vec<PrUnderReview>, Vec<SkippedPr>), String> {
        let mut out = Vec::new();
        let mut skipped = Vec::new();
        for (repo, number) in refs {
            let pr = self
                .github
                .pull_request(repo, *number)
                .await
                .map_err(|e| format!("pull request {repo}#{number} could not be read ({e})"))?;
            if !pr.open {
                continue;
            }
            if !self.author_allowed(&pr.author) {
                skipped.push(SkippedPr {
                    repo: pr.repo,
                    number: pr.number,
                    login: pr.author,
                });
                continue;
            }
            out.push(PrUnderReview {
                repo: pr.repo,
                number: pr.number,
                url: pr.url,
                head_sha: pr.head_sha,
                head_ref: pr.head_ref,
            });
        }
        Ok((out, skipped))
    }

    /// The open pull requests of `refs` by allowed authors, with their
    /// current heads.
    async fn read_prs(&self, refs: &[(String, u64)]) -> Result<Vec<PrUnderReview>, String> {
        self.read_pr_set(refs).await.map(|(prs, _)| prs)
    }

    /// The issue's open pull requests by allowed authors, with their current
    /// heads, and those left out because of their author.
    async fn open_prs(
        &self,
        issue: &ReviewIssue,
        extra: Option<(String, u64)>,
    ) -> Result<(Vec<PrUnderReview>, Vec<SkippedPr>), String> {
        let (refs, foreign) = self.split_refs(issue, extra);
        let out = self.read_pr_set(&refs).await?;
        if !foreign.is_empty() {
            let list = foreign
                .iter()
                .map(|(repo, n)| format!("{repo}#{n}"))
                .collect::<Vec<_>>()
                .join(", ");
            let ours = self.config.repo.as_deref().unwrap_or_default();
            tracing::warn!(issue = %issue.identifier, %list, "pull requests in another repository are not reviewed");
            self.post_or_log(
                &issue.id,
                &format!(
                    "Automatic review skipped {list}: only pull requests in {ours} are reviewed."
                ),
            )
            .await;
        }
        Ok(out)
    }

    fn in_review_state(&self, issue: &ReviewIssue) -> bool {
        issue.state_name.eq_ignore_ascii_case(&self.config.state)
    }

    /// No review: labels the pull requests and says so in Linear.
    async fn limit_reached(&self, issue: &ReviewIssue, state: &store::ReviewState) {
        let send_backs = state.send_backs;
        if state.limit_notice_sent_at.is_some() {
            tracing::info!(issue = %issue.identifier, send_backs, "limit notice already sent since the last send-back; nothing more posted");
            return;
        }
        metrics::counter!("conductor_review_limit_reached_total").increment(1);
        tracing::info!(issue = %issue.identifier, send_backs, "automatic review limit reached");
        match self.open_prs(issue, None).await {
            Ok((prs, _)) => {
                for pr in prs {
                    if let Err(e) = self
                        .github
                        .add_label(&pr.repo, pr.number, NEEDS_HUMAN_LABEL)
                        .await
                    {
                        tracing::error!(repo = %pr.repo, pr = pr.number, error = %e, "cannot label the pull request");
                    }
                }
            }
            Err(reason) => {
                tracing::warn!(issue = %issue.identifier, %reason, "cannot label the pull requests")
            }
        }
        let body = limit_comment(send_backs);
        if self
            .write_linear("limit notice", || self.linear.comment(&issue.id, &body))
            .await
            .is_ok()
        {
            if let Err(e) = store::mark_limit_notice(
                self.registry.pool(),
                &issue.id,
                &issue.identifier,
                Utc::now(),
            )
            .await
            {
                tracing::error!(error = %e, "cannot record the limit notice");
            }
        }
    }

    /// Retries a Linear write with backoff until it succeeds or the write
    /// budget is spent.
    async fn write_linear<F, Fut>(&self, what: &str, mut op: F) -> Result<(), LinearApiError>
    where
        F: FnMut() -> Fut,
        Fut: Future<Output = Result<(), LinearApiError>>,
    {
        let deadline = tokio::time::Instant::now() + self.config.linear_write_budget;
        let mut delay = self.config.linear_retry;
        loop {
            match op().await {
                Ok(()) => return Ok(()),
                Err(e) => {
                    if tokio::time::Instant::now() + delay >= deadline {
                        tracing::error!(what, error = %e, "Linear write kept failing; giving up");
                        return Err(e);
                    }
                    tracing::warn!(what, error = %e, retry_in_ms = delay.as_millis() as u64, "Linear write failed; retrying");
                    tokio::time::sleep(delay).await;
                    delay = (delay * 2).min(MAX_RETRY_DELAY);
                }
            }
        }
    }

    async fn post_or_log(&self, issue_id: &str, body: &str) {
        let _ = self
            .write_linear("review comment", || self.linear.comment(issue_id, body))
            .await;
    }

    /// Starts a pass in the background unless one is running. Called on every
    /// scheduler pass, which a `Stop` hook wakes.
    pub fn spawn_pass(self: &Arc<Self>) {
        if self
            .passing
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            return;
        }
        let me = Arc::clone(self);
        tokio::spawn(async move {
            me.pass().await;
            me.passing.store(false, Ordering::SeqCst);
        });
    }

    /// Looks at every review run and hands each one that is over to a task.
    pub async fn pass(self: &Arc<Self>) {
        let runs = match store::runs(self.registry.pool()).await {
            Ok(r) => r,
            Err(e) => {
                tracing::error!(error = %e, "cannot list review runs");
                return;
            }
        };
        for run in runs {
            if self
                .finishing
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .contains(&run.issue_id)
            {
                continue;
            }
            let group = match self.registry.group_by_slug(&run.group_slug).await {
                Ok(g) => g,
                Err(e) => {
                    tracing::error!(error = %e, "cannot read the review set");
                    continue;
                }
            };
            if run.cancelled_at.is_none()
                && group.as_ref().is_some_and(|g| self.in_progress(&run, g))
            {
                continue;
            }
            let me = Arc::clone(self);
            tokio::spawn(async move { me.finish(run, group).await });
        }
    }

    /// Whether the review's turn is still to come or still running.
    fn in_progress(&self, run: &ReviewRun, group: &Group) -> bool {
        matches!(
            turn_status(run, group, Utc::now(), self.config.timeout),
            TurnStatus::Running | TurnStatus::Queued
        )
    }

    /// Handles one review that is over: reads the verdict, does the side
    /// effects, closes the set and forgets the run.
    async fn finish(self: Arc<Self>, run: ReviewRun, group: Option<Group>) {
        let Some(_claim) = InFlight::claim(&self.finishing, &run.issue_id) else {
            return;
        };
        let outcome = match (&group, run.cancelled_at) {
            (_, Some(_)) => Outcome::Discarded {
                reason: "the issue left review".into(),
                requeue: false,
            },
            (None, None) => Outcome::Discarded {
                reason: "the review set was closed by hand".into(),
                requeue: false,
            },
            (Some(group), None) => self.guard_state(&run, self.judge(&run, group).await).await,
        };
        let key = GroupKey::Review(run.identifier.clone());
        let verdict_label = match &outcome {
            Outcome::Verdict(v) => v.verdict.as_str(),
            Outcome::Failed(_) => "failed",
            Outcome::Discarded { .. } => "discarded",
        };
        tracing::info!(issue = %run.identifier, outcome = verdict_label, "review finished");

        if let Outcome::Discarded { .. } = &outcome {
            if group
                .as_ref()
                .is_some_and(|g| matches!(g.state, GroupState::Busy { .. }))
            {
                if let Err(e) = self.runtime.interrupt(&key.slug()).await {
                    tracing::warn!(group = %key.slug(), error = %e, "cannot interrupt the review");
                }
            }
        }

        let recorded = match &outcome {
            Outcome::Verdict(verdict) => self.apply_verdict(&run, verdict).await,
            Outcome::Failed(reason) => {
                tracing::warn!(issue = %run.identifier, %reason, "review failed");
                self.post_or_log(&run.issue_id, &failure_comment(reason))
                    .await;
                "failed"
            }
            Outcome::Discarded { reason, .. } => {
                tracing::info!(issue = %run.identifier, %reason, "review discarded");
                "discarded"
            }
        };
        let counted = recorded != "discarded";
        if let Err(e) = store::record_review(
            self.registry.pool(),
            &run.issue_id,
            &run.identifier,
            run.trigger,
            recorded,
            counted,
            Utc::now(),
        )
        .await
        {
            tracing::error!(error = %e, "cannot record the review");
        }
        record_metric(run.trigger, recorded);
        metrics::histogram!("conductor_review_duration_seconds").record(
            (Utc::now() - run.created_at)
                .to_std()
                .unwrap_or_default()
                .as_secs_f64(),
        );

        self.close_set(&key).await;
        if let Err(e) = store::delete_run(self.registry.pool(), &run.issue_id).await {
            tracing::error!(error = %e, "cannot forget the review run");
        }
        drop(_claim);

        if let Outcome::Discarded { requeue: true, .. } = outcome {
            let again = self
                .trigger(&run.issue_id, run.trigger, true, run.extra.clone())
                .await;
            tracing::info!(issue = %run.identifier, outcome = ?again, "review queued again after a head change");
        }
        self.wake.notify_one();
    }

    /// An automatic review's verdict is acted on only while the issue is still
    /// in the review state: an issue moved on to Done or back to In Progress
    /// while the review ran must not be moved again.
    async fn guard_state(&self, run: &ReviewRun, outcome: Outcome) -> Outcome {
        if run.trigger != Trigger::Automatic || !matches!(outcome, Outcome::Verdict(_)) {
            return outcome;
        }
        match self.linear.issue(&run.issue_id).await {
            Ok(Some(issue)) if self.in_review_state(&issue) => outcome,
            Ok(Some(issue)) => Outcome::Discarded {
                reason: format!("the issue is now {}", issue.state_name),
                requeue: false,
            },
            Ok(None) => Outcome::Discarded {
                reason: "the issue no longer exists".into(),
                requeue: false,
            },
            Err(e) => Outcome::Failed(format!("the issue's state could not be read ({e})")),
        }
    }

    /// Decides what a finished turn amounts to.
    async fn judge(&self, run: &ReviewRun, group: &Group) -> Outcome {
        if turn_status(run, group, Utc::now(), self.config.timeout) != TurnStatus::Over {
            return Outcome::Failed("the review session did not start in time".into());
        }
        let timed_out = group
            .last_run
            .as_ref()
            .is_some_and(|r| r.result == RunResult::TimedOut && r.finished_at >= run.created_at);
        if timed_out {
            return Outcome::Failed(format!(
                "the review took longer than {} minutes and was interrupted",
                self.config.timeout.as_secs() / 60
            ));
        }
        for pr in &run.prs {
            match self.current_head(pr).await {
                Ok(head) if head != pr.head_sha => {
                    return Outcome::Discarded {
                        reason: format!("PR #{} has a new head", pr.number),
                        requeue: true,
                    }
                }
                Ok(_) => {}
                Err(reason) => return Outcome::Failed(reason),
            }
        }
        let path = if group.worktree.as_os_str().is_empty() {
            self.worktree_root.join(&run.group_slug)
        } else {
            group.worktree.clone()
        }
        .join(VERDICT_FILE);
        match read_verdict(&path, &run.prs) {
            Ok(v) => Outcome::Verdict(v),
            Err(reason) => Outcome::Failed(reason),
        }
    }

    async fn current_head(&self, pr: &PrUnderReview) -> Result<String, String> {
        let mut delay = Duration::from_secs(2);
        let mut attempt = 1;
        loop {
            match self.github.pull_request(&pr.repo, pr.number).await {
                Ok(now) => return Ok(now.head_sha),
                Err(e) if attempt >= HEAD_LOOKUP_ATTEMPTS => {
                    return Err(format!(
                        "PR #{}'s current head could not be read ({e})",
                        pr.number
                    ))
                }
                Err(e) => {
                    tracing::warn!(pr = pr.number, error = %e, "cannot read the pull request head; retrying");
                    tokio::time::sleep(delay).await;
                    delay *= 2;
                    attempt += 1;
                }
            }
        }
    }

    /// Does a verdict's side effects; returns the verdict label to record.
    async fn apply_verdict(&self, run: &ReviewRun, verdict: &Verdict) -> &'static str {
        match verdict.verdict {
            VerdictKind::ChangesRequested => {
                let send_backs = match store::state(self.registry.pool(), &run.issue_id).await {
                    Ok(s) => s.map(|s| s.send_backs).unwrap_or(0),
                    Err(e) => {
                        tracing::error!(error = %e, "cannot read review state");
                        return "failed";
                    }
                };
                let body = send_back_comment(
                    send_backs + 1,
                    self.config.max_iterations,
                    verdict,
                    &run.prs,
                );
                if self
                    .write_linear("send-back comment", || {
                        self.linear.comment(&run.issue_id, &body)
                    })
                    .await
                    .is_err()
                {
                    tracing::error!(issue = %run.identifier, "send-back comment could not be written to Linear");
                    return "failed";
                }
                if let Err(e) =
                    store::add_send_back(self.registry.pool(), &run.issue_id, &run.identifier).await
                {
                    tracing::error!(error = %e, "cannot count the send-back");
                }
                let moved = self
                    .write_linear("move to Todo", || async {
                        let issue =
                            self.linear.issue(&run.issue_id).await?.ok_or_else(|| {
                                LinearApiError::Malformed("issue not found".into())
                            })?;
                        self.linear.move_to_todo(&issue).await
                    })
                    .await;
                match moved {
                    Ok(()) => "changes_requested",
                    Err(e) => {
                        tracing::error!(issue = %run.identifier, error = %e, "the issue could not be moved to Todo");
                        let _ = self
                            .linear
                            .comment(
                                &run.issue_id,
                                &failure_comment("the issue could not be moved to Todo"),
                            )
                            .await;
                        "failed"
                    }
                }
            }
            VerdictKind::Approve => {
                let body = approval_comment(verdict, &run.prs);
                if self
                    .write_linear("approval comment", || {
                        self.linear.comment(&run.issue_id, &body)
                    })
                    .await
                    .is_err()
                {
                    return "failed";
                }
                for pr in &run.prs {
                    if let Err(e) = self
                        .github
                        .remove_label(&pr.repo, pr.number, NEEDS_HUMAN_LABEL)
                        .await
                    {
                        tracing::warn!(repo = %pr.repo, pr = pr.number, error = %e, "cannot remove the needs-human label");
                    }
                }
                "approve"
            }
        }
    }

    /// Closes a review set: the session, the worktree and the registry row
    /// go; the transcript stays.
    async fn close_set(&self, key: &GroupKey) {
        for attempt in 1..=CLOSE_ATTEMPTS {
            match close_group(
                &self.registry,
                &self.repo_root,
                key,
                CloseMode::Review,
                Some(self.runtime.as_ref()),
            )
            .await
            {
                Ok(CloseOutcome::Closed { .. } | CloseOutcome::Gone) => return,
                Ok(CloseOutcome::Busy) if attempt < CLOSE_ATTEMPTS => {
                    tokio::time::sleep(Duration::from_millis(200)).await;
                }
                Ok(other) => {
                    tracing::warn!(group = %key.slug(), outcome = ?other, "review set could not be closed");
                    return;
                }
                Err(e) => {
                    tracing::error!(group = %key.slug(), error = %e, "closing the review set failed");
                    return;
                }
            }
        }
    }
}

/// Where a review's turn stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TurnStatus {
    /// The turn is running.
    Running,
    /// The set is waiting for a review slot, or for its next start attempt.
    Queued,
    /// The set got a slot but the session never came up, or its starts keep
    /// failing.
    Stuck,
    /// Nothing is left to wait for; the verdict can be read.
    Over,
}

/// Classifies a review's set. The time a review waits behind other reviews for
/// a slot never counts: the stuck clock starts when the set is `Starting` or
/// `Delivering`, which it enters only with a slot. A set whose starts keep
/// failing is stuck once the review is older than `timeout` plus the grace.
pub fn turn_status(
    run: &ReviewRun,
    group: &Group,
    now: chrono::DateTime<Utc>,
    timeout: Duration,
) -> TurnStatus {
    let elapsed = |since: chrono::DateTime<Utc>| (now - since).to_std().unwrap_or_default();
    match group.state {
        GroupState::Busy { .. } => return TurnStatus::Running,
        GroupState::Starting { since } | GroupState::Delivering { since, .. } => {
            return if elapsed(since) > STUCK_GRACE {
                TurnStatus::Stuck
            } else {
                TurnStatus::Queued
            };
        }
        _ => {}
    }
    if group.pending.is_empty() {
        return TurnStatus::Over;
    }
    let start_failed = group
        .last_run
        .as_ref()
        .is_some_and(|r| r.result == RunResult::StartFailed && r.finished_at >= run.created_at);
    if start_failed && elapsed(run.created_at) > timeout + STUCK_GRACE {
        TurnStatus::Stuck
    } else {
        TurnStatus::Queued
    }
}

fn record_metric(trigger: Trigger, verdict: &'static str) {
    metrics::counter!("conductor_reviews_total", "trigger" => trigger.as_str(), "verdict" => verdict)
        .increment(1);
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[derive(Parser)]
    struct Probe {
        #[command(flatten)]
        review: ReviewArgs,
    }

    #[test]
    fn the_author_app_is_allowed_for_reviews_but_never_a_watched_user() {
        let probe = Probe::try_parse_from([
            "probe",
            "--author-bot-login",
            "nexus-mcp[bot]",
        ])
        .unwrap();
        let config = probe.review.into_config("reviewer".into(), None);
        assert!(config.allowed_authors.iter().any(|a| a == "nexus-mcp[bot]"));
        assert!(!crate::github::watched_users_from_env()
            .iter()
            .any(|a| a.eq_ignore_ascii_case("nexus-mcp[bot]")));
        let none = Probe::try_parse_from(["probe", "--author-bot-login", " "]).unwrap();
        let config = none.review.into_config("reviewer".into(), None);
        assert_eq!(config.author_bot_login, None);
        assert!(!config.allowed_authors.is_empty());
    }

    fn pr(number: u64, sha: &str) -> PrUnderReview {
        PrUnderReview {
            repo: "lassemand/nexus".into(),
            number,
            url: format!("https://github.com/lassemand/nexus/pull/{number}"),
            head_sha: sha.into(),
            head_ref: "feature/NEX-1-x".into(),
        }
    }

    #[test]
    fn durations_parse() {
        assert_eq!(parse_duration("45m"), Ok(Duration::from_secs(2700)));
        assert_eq!(parse_duration("45"), Ok(Duration::from_secs(2700)));
        assert_eq!(parse_duration("90s"), Ok(Duration::from_secs(90)));
        assert_eq!(parse_duration("1h"), Ok(Duration::from_secs(3600)));
        for bad in ["", "0", "m", "5d", "-1m", "1.5h"] {
            assert!(parse_duration(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn pr_urls_parse() {
        assert_eq!(
            parse_pr_url("https://github.com/lassemand/nexus/pull/316"),
            Some(("lassemand/nexus".into(), 316))
        );
        assert_eq!(
            parse_pr_url("https://github.com/lassemand/nexus/pull/316/files#diff"),
            Some(("lassemand/nexus".into(), 316))
        );
        assert_eq!(
            parse_pr_url("https://github.com/lassemand/nexus/issues/3"),
            None
        );
        assert_eq!(parse_pr_url("https://linear.app/x/issue/NEX-1"), None);
    }

    #[test]
    fn the_issue_comes_from_the_branch_then_the_body() {
        assert_eq!(
            issue_key_from_pr("feature/NEX-270-saxo", ""),
            Some("NEX-270".into())
        );
        assert_eq!(
            issue_key_from_pr("mogens/nex-272-conductor-review", ""),
            Some("NEX-272".into())
        );
        assert_eq!(
            issue_key_from_pr(
                "agent/review-agent",
                "See https://linear.app/nexusboard/issue/NEX-9/some-title for context"
            ),
            Some("NEX-9".into())
        );
        assert_eq!(issue_key_from_pr("agent/annex-12", "nothing"), None);
        assert_eq!(
            issue_key_from_pr("main", "mentions NEX-3 without a link"),
            None
        );
    }

    #[test]
    fn the_prompt_has_one_labelled_line_per_fact() {
        let request = ReviewRequest {
            identifier: "NEX-270".into(),
            issue_url: "https://linear.app/x/issue/NEX-270".into(),
            prs: vec![pr(316, "abc"), pr(317, "def")],
            review_number: 2,
            send_backs: 1,
            limit: 2,
            trigger: Trigger::Manual,
            verdict_path: "/w/review-nex-270/.review/verdict.json".into(),
        };
        let prompt = request.prompt();
        for line in [
            "Linear issue: NEX-270",
            "Linear URL: https://linear.app/x/issue/NEX-270",
            "PR: number=316 url=https://github.com/lassemand/nexus/pull/316 head_sha=abc",
            "PR: number=317 url=https://github.com/lassemand/nexus/pull/317 head_sha=def",
            "Review number: 2",
            "Send-backs so far: 1",
            "Send-back limit: 2",
            "Trigger: manual",
            "Verdict file: /w/review-nex-270/.review/verdict.json",
        ] {
            assert!(
                prompt.lines().any(|l| l == line),
                "missing {line:?} in\n{prompt}"
            );
        }
    }

    fn write(dir: &Path, body: &str) -> PathBuf {
        let path = dir.join("verdict.json");
        std::fs::write(&path, body).unwrap();
        path
    }

    #[test]
    fn verdicts_are_validated_against_the_prompt() {
        let dir = tempfile::tempdir().unwrap();
        let prs = [pr(316, "abc")];
        assert!(read_verdict(&dir.path().join("missing.json"), &prs)
            .unwrap_err()
            .contains("no verdict file"));
        let bad = write(dir.path(), "{not json");
        assert!(read_verdict(&bad, &prs).unwrap_err().contains("not valid"));
        let other_sha = write(
            dir.path(),
            r#"{"verdict":"approve","prs":[{"number":316,"head_sha":"zzz"}],"summary":"s"}"#,
        );
        assert!(read_verdict(&other_sha, &prs)
            .unwrap_err()
            .contains("does not match"));
        let missing_pr = write(
            dir.path(),
            r#"{"verdict":"approve","prs":[],"summary":"s"}"#,
        );
        assert!(read_verdict(&missing_pr, &prs)
            .unwrap_err()
            .contains("does not cover PR #316"));
        let good = write(
            dir.path(),
            r#"{"verdict":"changes_requested","prs":[{"number":316,"head_sha":"abc","review_id":123,"blocking":2,"non_blocking":3}],"summary":"Two bugs."}"#,
        );
        let v = read_verdict(&good, &prs).unwrap();
        assert_eq!(v.verdict, VerdictKind::ChangesRequested);
        let comment = send_back_comment(1, 2, &v, &prs);
        assert!(comment.starts_with("Review send-back 1 of 2\n"));
        assert!(comment.contains("Two bugs."));
        assert!(comment.contains(
            "https://github.com/lassemand/nexus/pull/316#pullrequestreview-123 (2 blocking, 3 non-blocking)"
        ));
    }

    #[test]
    fn comments_say_what_the_ticket_asks() {
        assert_eq!(
            limit_comment(2),
            "Automatic review limit reached (2 send-backs). Comment `/review` on the PR to request another review."
        );
        assert_eq!(
            failure_comment("the reviewer wrote no verdict file"),
            "Automatic review failed: the reviewer wrote no verdict file. Comment `/review` on the PR to retry."
        );
    }

    fn group(
        state: GroupState,
        pending: bool,
        last_run: Option<crate::registry::RunOutcome>,
    ) -> Group {
        let now = Utc::now();
        Group {
            key: GroupKey::review("NEX-1").unwrap(),
            session_id: uuid::Uuid::nil(),
            session_started: false,
            state,
            worktree: PathBuf::new(),
            worktree_ref: None,
            branches: Default::default(),
            issues: vec![],
            pending: if pending {
                std::collections::VecDeque::from([crate::registry::QueuedEvent {
                    id: 1,
                    event: InboundEvent::ReviewRequested {
                        request: ReviewRequest {
                            identifier: "NEX-1".into(),
                            issue_url: String::new(),
                            prs: vec![],
                            review_number: 1,
                            send_backs: 0,
                            limit: 2,
                            trigger: Trigger::Automatic,
                            verdict_path: String::new(),
                        },
                    },
                    received_at: now,
                }])
            } else {
                Default::default()
            },
            model: None,
            model_from_set: false,
            last_active: now,
            last_run,
            created_at: now,
            close_requested_at: None,
            close_blocked: None,
            delivery_blocked: None,
        }
    }

    fn run_created(at: chrono::DateTime<Utc>) -> ReviewRun {
        ReviewRun {
            issue_id: "i".into(),
            identifier: "NEX-1".into(),
            issue_url: String::new(),
            group_slug: "review-nex-1".into(),
            trigger: Trigger::Automatic,
            review_number: 1,
            prs: vec![],
            created_at: at,
            cancelled_at: None,
            extra: None,
        }
    }

    #[test]
    fn waiting_for_a_slot_never_counts_towards_the_stuck_deadline() {
        let timeout = Duration::from_secs(45 * 60);
        let now = Utc::now();
        let queued_for_hours = run_created(now - chrono::Duration::hours(3));
        let waiting = group(GroupState::NoSession, true, None);
        assert_eq!(
            turn_status(&queued_for_hours, &waiting, now, timeout),
            TurnStatus::Queued
        );

        let just_started = group(
            GroupState::Starting {
                since: now - chrono::Duration::seconds(30),
            },
            true,
            None,
        );
        assert_eq!(
            turn_status(&queued_for_hours, &just_started, now, timeout),
            TurnStatus::Queued,
            "the clock starts when the set got its slot"
        );

        let stuck = group(
            GroupState::Delivering {
                since: now - chrono::Duration::minutes(11),
                event_id: 1,
            },
            true,
            None,
        );
        assert_eq!(
            turn_status(&queued_for_hours, &stuck, now, timeout),
            TurnStatus::Stuck
        );
    }

    #[test]
    fn a_set_whose_starts_keep_failing_is_stuck_once_the_review_is_old_enough() {
        let timeout = Duration::from_secs(45 * 60);
        let now = Utc::now();
        let failed = |at: chrono::DateTime<Utc>| crate::registry::RunOutcome {
            result: RunResult::StartFailed,
            finished_at: at,
            duration: Duration::from_secs(1),
            model: None,
        };
        let young = run_created(now - chrono::Duration::minutes(10));
        let old = run_created(now - chrono::Duration::minutes(60));
        let g = group(GroupState::NoSession, true, Some(failed(now)));
        assert_eq!(turn_status(&young, &g, now, timeout), TurnStatus::Queued);
        assert_eq!(turn_status(&old, &g, now, timeout), TurnStatus::Stuck);
        let earlier = group(
            GroupState::NoSession,
            true,
            Some(failed(now - chrono::Duration::hours(5))),
        );
        assert_eq!(
            turn_status(&old, &earlier, now, timeout),
            TurnStatus::Queued,
            "a failure from before this review says nothing about it"
        );
    }

    #[test]
    fn a_finished_turn_is_over_and_a_busy_one_is_running() {
        let timeout = Duration::from_secs(60);
        let now = Utc::now();
        let run = run_created(now);
        let ready = group(
            GroupState::Ready {
                pane_pid: 1,
                since: now,
            },
            false,
            None,
        );
        assert_eq!(turn_status(&run, &ready, now, timeout), TurnStatus::Over);
        let busy = group(
            GroupState::Busy {
                since: now,
                delivered_event: Some(1),
            },
            false,
            None,
        );
        assert_eq!(turn_status(&run, &busy, now, timeout), TurnStatus::Running);
    }

    #[test]
    fn remotes_name_their_repository() {
        for url in [
            "https://github.com/lassemand/nexus.git",
            "https://github.com/lassemand/nexus",
            "https://x-access-token:abc@github.com/lassemand/nexus.git\n",
            "git@github.com:lassemand/nexus.git",
        ] {
            assert_eq!(
                github_repo_from_remote(url),
                Some("lassemand/nexus".into()),
                "{url}"
            );
        }
        assert_eq!(github_repo_from_remote("/srv/git/origin.git"), None);
        assert_eq!(github_repo_from_remote("https://gitlab.com/a/b.git"), None);
    }
}
