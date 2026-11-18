//! The CI gate: no automatic review starts until every check run on the head
//! of every open pull request of the issue has passed.
//!
//! An issue that enters review earlier is bounced: to In Progress while checks
//! are unfinished (or cannot be read), to Todo when one failed. A bounced,
//! unfinished issue is watched (table `ci_watch`); when its checks turn green
//! conductor moves it back into review itself, and that move runs through the
//! gate again like any other. Events (`check_suite`, `check_run`, a push)
//! trigger a re-check at once, a poll catches lost webhooks.

use std::sync::Arc;

use chrono::Utc;

use super::store::{self, CiWatch};
use super::{InFlight, PrUnderReview, ReviewIssue, Reviews, TriggerOutcome};
use crate::linear_api::LinearApiError;

/// Linear state an issue with unfinished checks is moved to.
pub const IN_PROGRESS_STATE: &str = "In Progress";

/// Conclusions that count as passed.
const PASSING: [&str; 3] = ["success", "neutral", "skipped"];

/// One check run reported for a commit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckRun {
    pub name: String,
    /// `queued`, `in_progress`, `completed`, and so on.
    pub status: String,
    pub conclusion: Option<String>,
    /// Link to the run's page and log.
    pub url: String,
}

/// A failed check, for the Linear comment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FailedCheck {
    pub repo: String,
    pub pr: u64,
    pub name: String,
    pub url: String,
}

/// What the checks of one or more pull requests add up to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Gate {
    /// Every check run of every pull request passed.
    Green,
    /// Something is unfinished, nothing has registered yet, or GitHub could
    /// not be read. `unfinished` is `(repo, number)` of those pull requests.
    Pending {
        unfinished: Vec<(String, u64)>,
        errored: bool,
    },
    /// At least one check failed.
    Failed(Vec<FailedCheck>),
}

/// The state of one commit's check runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommitChecks {
    Green,
    Pending,
    /// `(name, url)` of each failed check.
    Failed(Vec<(String, String)>),
}

/// Adds up a commit's check runs. No check runs at all is pending: CI has not
/// registered yet. A failure beats a check that is still running.
pub fn classify(runs: &[CheckRun]) -> CommitChecks {
    if runs.is_empty() {
        return CommitChecks::Pending;
    }
    let failed: Vec<(String, String)> = runs
        .iter()
        .filter(|r| {
            r.status == "completed"
                && !r
                    .conclusion
                    .as_deref()
                    .is_some_and(|c| PASSING.contains(&c))
        })
        .map(|r| (r.name.clone(), r.url.clone()))
        .collect();
    if !failed.is_empty() {
        return CommitChecks::Failed(failed);
    }
    if runs.iter().any(|r| r.status != "completed") {
        return CommitChecks::Pending;
    }
    CommitChecks::Green
}

/// The Linear comment for unfinished checks, one line per pull request.
pub fn pending_comment(unfinished: &[(String, u64)]) -> String {
    unfinished
        .iter()
        .map(|(repo, n)| format!("CI not finished on {repo} PR {n}; moved back until it passes."))
        .collect::<Vec<_>>()
        .join("\n")
}

/// The Linear comment for failed checks, each with a link to its log.
pub fn failed_comment(failed: &[FailedCheck]) -> String {
    let mut lines = vec!["CI failed; moved back to Todo. Failed checks:".to_string()];
    for f in failed {
        lines.push(format!("- {} PR {}: [{}]({})", f.repo, f.pr, f.name, f.url));
    }
    lines.join("\n")
}

/// The Linear comment when the wait for CI ran out.
pub fn timeout_comment(unfinished: &[(String, u64)], waited: std::time::Duration) -> String {
    let list = unfinished
        .iter()
        .map(|(repo, n)| format!("{repo} PR {n}"))
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "CI still not finished on {list} after {} minutes; conductor stops watching it. \
         The issue stays in {IN_PROGRESS_STATE}; move it to review again once CI is green.",
        waited.as_secs() / 60
    )
}

fn closed_state(name: &str) -> bool {
    ["done", "canceled", "cancelled", "duplicate"]
        .iter()
        .any(|s| name.eq_ignore_ascii_case(s))
}

fn count_gate(outcome: &'static str) {
    metrics::counter!("conductor_ci_gate_total", "outcome" => outcome).increment(1);
}

impl Reviews {
    /// Reads the checks on the head of each pull request.
    async fn check_prs(&self, prs: &[PrUnderReview]) -> Gate {
        let mut unfinished = Vec::new();
        let mut failed = Vec::new();
        let mut errored = false;
        for pr in prs {
            if pr.head_sha.is_empty() {
                unfinished.push((pr.repo.clone(), pr.number));
                continue;
            }
            match self.github.check_runs(&pr.repo, &pr.head_sha).await {
                Ok(runs) => match classify(&runs) {
                    CommitChecks::Green => {}
                    CommitChecks::Pending => unfinished.push((pr.repo.clone(), pr.number)),
                    CommitChecks::Failed(list) => {
                        failed.extend(list.into_iter().map(|(name, url)| FailedCheck {
                            repo: pr.repo.clone(),
                            pr: pr.number,
                            name,
                            url,
                        }))
                    }
                },
                Err(e) => {
                    tracing::warn!(repo = %pr.repo, pr = pr.number, error = %e, "cannot read the check runs; treated as not green");
                    errored = true;
                    unfinished.push((pr.repo.clone(), pr.number));
                }
            }
        }
        if !failed.is_empty() {
            Gate::Failed(failed)
        } else if !unfinished.is_empty() {
            Gate::Pending {
                unfinished,
                errored,
            }
        } else {
            Gate::Green
        }
    }

    fn record_gate(gate: &Gate) {
        count_gate(match gate {
            Gate::Green => "green",
            Gate::Pending { errored: true, .. } => "github_error",
            Gate::Pending { .. } => "pending",
            Gate::Failed(_) => "failed",
        });
    }

    /// The first check of an automatic trigger: passes only on green, else
    /// bounces the issue.
    pub(super) async fn require_green(
        &self,
        issue: &ReviewIssue,
        prs: &[PrUnderReview],
    ) -> Result<(), TriggerOutcome> {
        match self.check_prs(prs).await {
            Gate::Green => Ok(()),
            other => {
                Self::record_gate(&other);
                Err(self.bounce(issue, prs, other).await)
            }
        }
    }

    /// The check right before queueing: the heads are read again, so a push
    /// since the first check means a gate on the new head. Returns the pull
    /// requests as read now, which the review then names.
    pub(super) async fn reconfirm_green(
        &self,
        issue: &ReviewIssue,
        extra: Option<(String, u64)>,
    ) -> Result<Vec<PrUnderReview>, TriggerOutcome> {
        let (refs, _) = self.split_refs(issue, extra);
        let fresh = match self.read_prs(&refs).await {
            Ok(p) => p,
            Err(reason) => {
                tracing::warn!(issue = %issue.identifier, %reason, "pull requests unreadable before queueing; CI cannot be confirmed");
                return Err(self.bounce_unconfirmed(issue, &refs).await);
            }
        };
        if fresh.is_empty() {
            return Err(TriggerOutcome::NoPr);
        }
        let gate = self.check_prs(&fresh).await;
        Self::record_gate(&gate);
        match gate {
            Gate::Green => Ok(fresh),
            other => Err(self.bounce(issue, &fresh, other).await),
        }
    }

    /// GitHub could not be read: not green, so the issue is bounced and watched.
    pub(super) async fn bounce_unconfirmed(
        &self,
        issue: &ReviewIssue,
        refs: &[(String, u64)],
    ) -> TriggerOutcome {
        count_gate("github_error");
        let prs: Vec<PrUnderReview> = refs
            .iter()
            .map(|(repo, number)| PrUnderReview {
                repo: repo.clone(),
                number: *number,
                url: String::new(),
                head_sha: String::new(),
                head_ref: String::new(),
            })
            .collect();
        self.bounce_pending(issue, &prs, refs).await
    }

    async fn bounce(
        &self,
        issue: &ReviewIssue,
        prs: &[PrUnderReview],
        gate: Gate,
    ) -> TriggerOutcome {
        match gate {
            Gate::Green => TriggerOutcome::Queued,
            Gate::Pending { unfinished, .. } => self.bounce_pending(issue, prs, &unfinished).await,
            Gate::Failed(failed) => {
                self.send_to_todo(issue, &failed).await;
                TriggerOutcome::CiFailed
            }
        }
    }

    async fn bounce_pending(
        &self,
        issue: &ReviewIssue,
        prs: &[PrUnderReview],
        unfinished: &[(String, u64)],
    ) -> TriggerOutcome {
        let pool = self.registry.pool();
        let watch = CiWatch {
            issue_id: issue.id.clone(),
            identifier: issue.identifier.clone(),
            prs: prs.to_vec(),
            created_at: Utc::now(),
        };
        if let Err(e) = store::upsert_watch(pool, &watch).await {
            tracing::error!(issue = %issue.identifier, error = %e, "cannot record the CI watch");
        }
        self.publish_watch_gauge().await;
        if self
            .write_linear("move to In Progress", || {
                self.linear.move_to_state(issue, IN_PROGRESS_STATE)
            })
            .await
            .is_err()
        {
            tracing::error!(issue = %issue.identifier, "the issue could not be moved out of review");
            self.drop_watch(&issue.id, "the move out of review failed")
                .await;
            return TriggerOutcome::CiPending;
        }
        tracing::info!(issue = %issue.identifier, "CI not green; issue moved to In Progress and watched");
        self.post_or_log(&issue.id, &pending_comment(unfinished))
            .await;
        TriggerOutcome::CiPending
    }

    async fn send_to_todo(&self, issue: &ReviewIssue, failed: &[FailedCheck]) -> bool {
        let pool = self.registry.pool();
        let watch = store::watch(pool, &issue.id).await.ok().flatten();
        let _ = store::delete_watch(pool, &issue.id).await;
        if self
            .write_linear("move to Todo", || self.linear.move_to_todo(issue))
            .await
            .is_err()
        {
            tracing::error!(issue = %issue.identifier, "the issue could not be moved to Todo after failed CI");
            if let Some(w) = watch {
                let _ = store::upsert_watch(pool, &w).await;
            }
            self.publish_watch_gauge().await;
            return false;
        }
        tracing::info!(issue = %issue.identifier, failed = failed.len(), "CI failed; issue moved to Todo");
        self.publish_watch_gauge().await;
        self.post_or_log(&issue.id, &failed_comment(failed)).await;
        true
    }

    async fn publish_watch_gauge(&self) {
        if let Ok(all) = store::watches(self.registry.pool()).await {
            metrics::gauge!("conductor_ci_watch_issues").set(all.len() as f64);
        }
    }

    /// Stops watching an issue, if it is watched.
    pub(super) async fn drop_watch(&self, issue_id: &str, why: &str) {
        match store::delete_watch(self.registry.pool(), issue_id).await {
            Ok(true) => {
                tracing::info!(issue_id, why, "CI watch dropped");
                self.publish_watch_gauge().await;
            }
            Ok(false) => {}
            Err(e) => tracing::error!(issue_id, error = %e, "cannot drop the CI watch"),
        }
    }

    /// A check finished: re-checks the watched issues with that commit.
    pub(super) async fn on_checks_completed(
        &self,
        repo: &str,
        head_sha: &str,
        delivery_id: Option<String>,
    ) {
        if !self.first_delivery(delivery_id.as_deref()).await {
            return;
        }
        let watches = match store::watches(self.registry.pool()).await {
            Ok(w) => w,
            Err(e) => {
                tracing::error!(error = %e, "cannot read the CI watches");
                return;
            }
        };
        for watch in watches {
            let affected = watch.prs.iter().any(|p| {
                p.repo.eq_ignore_ascii_case(repo)
                    && (p.head_sha.is_empty() || p.head_sha.eq_ignore_ascii_case(head_sha))
            });
            if affected {
                self.evaluate_watch(&watch.issue_id).await;
            }
        }
    }

    /// A push: the watch moves to the new head and its wait starts over.
    pub(super) async fn on_pr_pushed(
        &self,
        repo: &str,
        pr_number: u64,
        head_sha: &str,
        delivery_id: Option<String>,
    ) {
        if !self.first_delivery(delivery_id.as_deref()).await {
            return;
        }
        let pool = self.registry.pool();
        let watches = match store::watches(pool).await {
            Ok(w) => w,
            Err(e) => {
                tracing::error!(error = %e, "cannot read the CI watches");
                return;
            }
        };
        for mut watch in watches {
            let mut touched = false;
            for pr in &mut watch.prs {
                if pr.repo.eq_ignore_ascii_case(repo) && pr.number == pr_number {
                    pr.head_sha = head_sha.to_string();
                    touched = true;
                }
            }
            if touched {
                tracing::info!(issue = %watch.identifier, pr = pr_number, "new head on a watched pull request; the watch starts over");
                if let Err(e) =
                    store::update_watch_prs(pool, &watch.issue_id, &watch.prs, Some(Utc::now()))
                        .await
                {
                    tracing::error!(error = %e, "cannot update the CI watch");
                }
            }
        }
    }

    /// Checks every watched issue every `CONDUCTOR_CI_POLL_INTERVAL`, in case
    /// a webhook was lost.
    pub async fn run_ci_poll(self: Arc<Self>) {
        loop {
            self.poll_watches().await;
            tokio::time::sleep(self.config.ci_poll_interval).await;
        }
    }

    /// One poll over all watches.
    pub async fn poll_watches(&self) {
        let watches = match store::watches(self.registry.pool()).await {
            Ok(w) => w,
            Err(e) => {
                tracing::error!(error = %e, "cannot read the CI watches");
                return;
            }
        };
        metrics::gauge!("conductor_ci_watch_issues").set(watches.len() as f64);
        for watch in watches {
            self.evaluate_watch(&watch.issue_id).await;
        }
    }

    /// Acts on one watched issue: promotes it on green, sends it to Todo on a
    /// failure, gives up after the maximum wait, and leaves it alone when it
    /// was moved by hand.
    pub async fn evaluate_watch(&self, issue_id: &str) {
        let Some(_claim) = InFlight::claim(&self.watching, issue_id) else {
            return;
        };
        let pool = self.registry.pool();
        let watch = match store::watch(pool, issue_id).await {
            Ok(Some(w)) => w,
            Ok(None) => return,
            Err(e) => {
                tracing::error!(error = %e, "cannot read the CI watch");
                return;
            }
        };
        let issue = match self.linear.issue(issue_id).await {
            Ok(Some(i)) => i,
            Ok(None) => {
                self.drop_watch(issue_id, "the issue no longer exists")
                    .await;
                return;
            }
            Err(e) => {
                tracing::warn!(issue = %watch.identifier, error = %e, "cannot read the issue; will retry");
                return;
            }
        };
        if closed_state(&issue.state_name) {
            self.drop_watch(issue_id, "the issue was closed").await;
            return;
        }
        if !issue.state_name.eq_ignore_ascii_case(IN_PROGRESS_STATE) {
            let waited = (Utc::now() - watch.created_at).to_std().unwrap_or_default();
            if waited >= self.config.ci_max_wait {
                self.drop_watch(issue_id, "the issue was moved by hand and the wait is over")
                    .await;
            } else {
                tracing::info!(issue = %watch.identifier, state = %issue.state_name, "watched issue is not in progress; left alone");
            }
            return;
        }
        let refs: Vec<(String, u64)> = watch
            .prs
            .iter()
            .map(|p| (p.repo.clone(), p.number))
            .collect();
        let (gate, prs) = match self.read_prs(&refs).await {
            Ok(prs) if prs.is_empty() => {
                self.drop_watch(issue_id, "its pull requests are closed")
                    .await;
                return;
            }
            Ok(prs) => (self.check_prs(&prs).await, prs),
            Err(reason) => {
                tracing::warn!(issue = %watch.identifier, %reason, "pull requests unreadable; treated as not green");
                (
                    Gate::Pending {
                        unfinished: refs.clone(),
                        errored: true,
                    },
                    watch.prs.clone(),
                )
            }
        };
        match gate {
            Gate::Green => self.promote(&issue, &watch).await,
            Gate::Failed(failed) => {
                count_gate("failed");
                self.send_to_todo(&issue, &failed).await;
            }
            Gate::Pending {
                unfinished,
                errored,
            } => {
                if errored {
                    count_gate("github_error");
                }
                let waited = (Utc::now() - watch.created_at).to_std().unwrap_or_default();
                if waited >= self.config.ci_max_wait {
                    count_gate("timeout");
                    self.drop_watch(issue_id, "the wait for CI timed out").await;
                    self.post_or_log(issue_id, &timeout_comment(&unfinished, waited))
                        .await;
                    return;
                }
                if prs != watch.prs && !errored {
                    if let Err(e) = store::update_watch_prs(pool, issue_id, &prs, None).await {
                        tracing::error!(error = %e, "cannot update the CI watch");
                    }
                }
            }
        }
    }

    /// Moves a watched issue into review; the move's webhook runs the gate again.
    async fn promote(&self, issue: &ReviewIssue, watch: &CiWatch) {
        let pool = self.registry.pool();
        let _ = store::delete_watch(pool, &issue.id).await;
        let state = self.config.state.clone();
        let moved: Result<(), LinearApiError> = self
            .write_linear("promote to review", || {
                self.linear.move_to_state(issue, &state)
            })
            .await;
        match moved {
            Ok(()) => {
                count_gate("promoted");
                tracing::info!(issue = %issue.identifier, "CI green; issue moved to review");
            }
            Err(e) => {
                tracing::error!(issue = %issue.identifier, error = %e, "cannot move the issue to review; keeps watching");
                let _ = store::upsert_watch(pool, watch).await;
            }
        }
        self.publish_watch_gauge().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(status: &str, conclusion: Option<&str>) -> CheckRun {
        CheckRun {
            name: "build".into(),
            status: status.into(),
            conclusion: conclusion.map(str::to_string),
            url: "https://github.com/o/r/runs/1".into(),
        }
    }

    #[test]
    fn no_check_runs_is_pending() {
        assert_eq!(classify(&[]), CommitChecks::Pending);
    }

    #[test]
    fn success_neutral_and_skipped_are_green() {
        let runs = [
            run("completed", Some("success")),
            run("completed", Some("neutral")),
            run("completed", Some("skipped")),
        ];
        assert_eq!(classify(&runs), CommitChecks::Green);
    }

    #[test]
    fn an_unfinished_run_is_pending() {
        let runs = [run("completed", Some("success")), run("in_progress", None)];
        assert_eq!(classify(&runs), CommitChecks::Pending);
    }

    #[test]
    fn a_failure_beats_a_running_check() {
        let runs = [run("in_progress", None), run("completed", Some("failure"))];
        assert!(matches!(classify(&runs), CommitChecks::Failed(f) if f.len() == 1));
    }

    #[test]
    fn cancelled_and_timed_out_runs_fail() {
        for c in ["cancelled", "timed_out", "action_required", "stale"] {
            assert!(matches!(
                classify(&[run("completed", Some(c))]),
                CommitChecks::Failed(_)
            ));
        }
    }

    #[test]
    fn the_pending_comment_names_each_pull_request() {
        assert_eq!(
            pending_comment(&[("o/r".into(), 7)]),
            "CI not finished on o/r PR 7; moved back until it passes."
        );
    }

    #[test]
    fn the_failed_comment_links_each_log() {
        let body = failed_comment(&[FailedCheck {
            repo: "o/r".into(),
            pr: 7,
            name: "build".into(),
            url: "https://github.com/o/r/runs/1".into(),
        }]);
        assert!(body.contains("[build](https://github.com/o/r/runs/1)"));
    }

    #[test]
    fn closed_states_are_recognised() {
        assert!(closed_state("Done"));
        assert!(closed_state("Canceled"));
        assert!(!closed_state("In Progress"));
    }
}
