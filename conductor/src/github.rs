//! GitHub webhook payloads, signature verification and comment filtering.

use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::Sha256;

/// Which GitHub event produced a pull request comment.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PrCommentKind {
    /// `issue_comment` on a pull request — a top-level conversation comment.
    Comment,
    /// `pull_request_review_comment` — an inline comment on a diff line.
    ReviewComment,
}

impl PrCommentKind {
    /// Stable identifier used in logs and, later, in dispatch routing.
    pub fn as_str(self) -> &'static str {
        match self {
            PrCommentKind::Comment => "pr_comment",
            PrCommentKind::ReviewComment => "pr_review_comment",
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct GithubUser {
    pub login: String,
}

#[derive(Debug, Deserialize)]
pub struct GithubRepository {
    pub full_name: String,
}

/// Payload for the `issue_comment` event. GitHub fires this for comments on both
/// plain issues and PRs — `issue.pull_request` is only `Some` when the comment is
/// actually on a PR, which is how the two are told apart.
#[derive(Debug, Deserialize)]
pub struct IssueCommentPayload {
    pub action: Option<String>,
    pub issue: Option<IssueCommentIssue>,
    pub comment: Option<IssueCommentBody>,
    pub repository: Option<GithubRepository>,
}

#[derive(Debug, Deserialize)]
pub struct IssueCommentIssue {
    pub number: u64,
    pub pull_request: Option<Value>,
}

#[derive(Debug, Deserialize)]
pub struct IssueCommentBody {
    pub id: u64,
    pub body: String,
    pub user: GithubUser,
}

/// Payload for `pull_request_review_comment` — an inline comment on a diff line.
#[derive(Debug, Deserialize)]
pub struct ReviewCommentPayload {
    pub action: Option<String>,
    pub pull_request: Option<ReviewCommentPr>,
    pub comment: Option<ReviewCommentBody>,
    pub repository: Option<GithubRepository>,
}

#[derive(Debug, Deserialize)]
pub struct ReviewCommentPr {
    pub number: u64,
    pub head: Option<PrBranch>,
    pub base: Option<PrBranch>,
}

/// One side of a pull request.
///
/// `repo` is absent when the fork it pointed at has been deleted, so both
/// fields are optional rather than assumed present.
#[derive(Debug, Deserialize)]
pub struct PrBranch {
    #[serde(rename = "ref")]
    pub ref_name: Option<String>,
    pub repo: Option<GithubRepository>,
}

impl PrBranch {
    /// The repository this side lives in, if known.
    fn full_name(&self) -> Option<String> {
        self.repo.as_ref().map(|r| r.full_name.clone())
    }
}

#[derive(Debug, Deserialize)]
pub struct ReviewCommentBody {
    pub id: u64,
    pub body: String,
    pub user: GithubUser,
    pub path: Option<String>,
}

/// Unified shape for a PR comment regardless of which GitHub event produced it.
///
/// Intentionally minimal — only what a consumer needs in order to act: where the
/// PR lives, which comment to reply to, what was said, and (for review comments)
/// which file the comment is on.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct GithubPrComment {
    pub repo: String,
    pub pr_number: u64,
    pub comment_id: u64,
    pub body: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file_path: Option<String>,
    /// Head branch of the pull request, when the payload carried it.
    ///
    /// Only `pull_request_review_comment` includes it; for `issue_comment` the
    /// branch has to be fetched. Defaulted so events queued before this field
    /// existed still deserialise.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub head_ref: Option<String>,
    /// Repository the head branch lives in, used to detect forks.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub head_repo: Option<String>,
    /// Repository the pull request targets.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_repo: Option<String>,
}

/// Default watched GitHub login when `GITHUB_WEBHOOK_USERS` is unset.
const DEFAULT_WATCHED_USER: &str = "lassemand";

/// Parses the comma-separated `GITHUB_WEBHOOK_USERS` value, falling back to the
/// default single login. Only comments from these users are dispatched —
/// otherwise every human exchange on a PR would wake a consumer.
pub fn watched_users_from_env() -> Vec<String> {
    std::env::var("GITHUB_WEBHOOK_USERS")
        .ok()
        .map(|v| {
            v.split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect::<Vec<_>>()
        })
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| vec![DEFAULT_WATCHED_USER.to_string()])
}

/// Whether `login` is one of the watched users (case-insensitive).
pub fn is_watched_user(watched: &[String], login: &str) -> bool {
    watched.iter().any(|u| u.eq_ignore_ascii_case(login))
}

/// Verifies GitHub's `X-Hub-Signature-256` header: `sha256=<hex hmac>` of the raw
/// request body, keyed by the webhook secret. Must run against the *raw* bytes,
/// before any JSON parsing.
pub fn verify_github_signature(secret: &str, signature_header: &str, body: &[u8]) -> bool {
    let Some(sig_hex) = signature_header.strip_prefix("sha256=") else {
        return false;
    };
    let Ok(sig_bytes) = hex::decode(sig_hex) else {
        return false;
    };
    let Ok(mut mac) = Hmac::<Sha256>::new_from_slice(secret.as_bytes()) else {
        return false;
    };
    mac.update(body);
    mac.verify_slice(&sig_bytes).is_ok()
}

/// Reduces an `issue_comment` payload to a dispatchable comment, or `None` if it
/// should be ignored (not newly created, not on a PR, or not from a watched user).
pub fn dispatchable_issue_comment(
    payload: IssueCommentPayload,
    watched: &[String],
) -> Option<GithubPrComment> {
    if payload.action.as_deref() != Some("created") {
        return None;
    }
    let issue = payload.issue?;
    // `issue_comment` fires for plain issues too; only PRs carry `pull_request`.
    issue.pull_request.as_ref()?;
    let comment = payload.comment?;
    if !is_watched_user(watched, &comment.user.login) {
        return None;
    }
    Some(GithubPrComment {
        repo: payload.repository.map(|r| r.full_name).unwrap_or_default(),
        pr_number: issue.number,
        comment_id: comment.id,
        body: comment.body,
        file_path: None,
        // `issue_comment` does not carry the branch; it is fetched when routing.
        head_ref: None,
        head_repo: None,
        base_repo: None,
    })
}

/// Reduces a `pull_request_review_comment` payload to a dispatchable comment, or
/// `None` if it should be ignored.
pub fn dispatchable_review_comment(
    payload: ReviewCommentPayload,
    watched: &[String],
) -> Option<GithubPrComment> {
    if payload.action.as_deref() != Some("created") {
        return None;
    }
    let pr = payload.pull_request?;
    let comment = payload.comment?;
    if !is_watched_user(watched, &comment.user.login) {
        return None;
    }
    Some(GithubPrComment {
        repo: payload.repository.map(|r| r.full_name).unwrap_or_default(),
        pr_number: pr.number,
        comment_id: comment.id,
        body: comment.body,
        file_path: comment.path,
        head_ref: pr.head.as_ref().and_then(|h| h.ref_name.clone()),
        head_repo: pr.head.as_ref().and_then(PrBranch::full_name),
        base_repo: pr.base.as_ref().and_then(PrBranch::full_name),
    })
}

/// A manual request for a review: a `/review` comment, or the reviewer app
/// (re-)requested as a reviewer on the pull request.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ReviewCommand {
    pub repo: String,
    pub pr_number: u64,
    /// The `/review` comment to react to; `None` for a review request.
    #[serde(default)]
    pub comment_id: Option<u64>,
    /// Who asked.
    pub login: String,
}

/// Whether a comment body asks for a review: it starts with `/review`
/// (case-insensitive, after trimming), followed by nothing or whitespace.
pub fn is_review_command(body: &str) -> bool {
    let body = body.trim();
    let Some(head) = body.get(..7) else {
        return false;
    };
    head.eq_ignore_ascii_case("/review") && body[7..].chars().next().is_none_or(char::is_whitespace)
}

/// Reduces an `issue_comment` payload to a review request, or `None` if it is
/// not a newly created `/review` comment on a pull request from a watched user.
pub fn review_command(payload: &IssueCommentPayload, watched: &[String]) -> Option<ReviewCommand> {
    if payload.action.as_deref() != Some("created") {
        return None;
    }
    let issue = payload.issue.as_ref()?;
    issue.pull_request.as_ref()?;
    let comment = payload.comment.as_ref()?;
    if !is_watched_user(watched, &comment.user.login) || !is_review_command(&comment.body) {
        return None;
    }
    Some(ReviewCommand {
        repo: payload
            .repository
            .as_ref()
            .map(|r| r.full_name.clone())
            .unwrap_or_default(),
        pr_number: issue.number,
        comment_id: Some(comment.id),
        login: comment.user.login.clone(),
    })
}

/// Payload for the `pull_request` event; only what a review request needs.
#[derive(Debug, Deserialize)]
pub struct PullRequestEventPayload {
    pub action: Option<String>,
    pub number: Option<u64>,
    /// Present on `review_requested` for a single reviewer (absent for a team).
    pub requested_reviewer: Option<GithubUser>,
    pub sender: Option<GithubUser>,
    pub repository: Option<GithubRepository>,
}

/// Reduces a `pull_request` event to a review request when a watched user
/// requested (or re-requested) the reviewer app as a reviewer.
///
/// GitHub only offers an app in the reviewer list once it has reviewed the
/// pull request, so this is the "Re-request review" button next to the
/// reviewer after its first, automatic review.
pub fn review_request(
    payload: &PullRequestEventPayload,
    watched: &[String],
    bot_login: Option<&str>,
) -> Option<ReviewCommand> {
    if payload.action.as_deref() != Some("review_requested") {
        return None;
    }
    let reviewer = payload.requested_reviewer.as_ref()?;
    if !is_review_bot(bot_login, &reviewer.login) {
        return None;
    }
    let sender = payload.sender.as_ref()?;
    if !is_watched_user(watched, &sender.login) {
        return None;
    }
    Some(ReviewCommand {
        repo: payload
            .repository
            .as_ref()
            .map(|r| r.full_name.clone())
            .unwrap_or_default(),
        pr_number: payload.number?,
        comment_id: None,
        login: sender.login.clone(),
    })
}

/// Whether `login` is the reviewer bot, whose comments never reach a session.
pub fn is_review_bot(bot_login: Option<&str>, login: &str) -> bool {
    bot_login.is_some_and(|bot| !bot.is_empty() && bot.eq_ignore_ascii_case(login))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn watched() -> Vec<String> {
        vec!["lassemand".to_string()]
    }

    fn issue_comment(action: &str, login: &str, is_pr: bool) -> IssueCommentPayload {
        let mut issue = json!({ "number": 7 });
        if is_pr {
            issue["pull_request"] = json!({ "url": "https://api.github.com/..." });
        }
        serde_json::from_value(json!({
            "action": action,
            "issue": issue,
            "comment": { "id": 42, "body": "hi", "user": { "login": login } },
            "repository": { "full_name": "lassemand/nexus" }
        }))
        .expect("valid payload")
    }

    #[test]
    fn created_pr_comment_from_watched_user_dispatches() {
        let got =
            dispatchable_issue_comment(issue_comment("created", "lassemand", true), &watched())
                .expect("dispatchable");
        assert_eq!(got.repo, "lassemand/nexus");
        assert_eq!(got.pr_number, 7);
        assert_eq!(got.comment_id, 42);
        assert_eq!(got.file_path, None);
    }

    #[test]
    fn comment_on_plain_issue_is_ignored() {
        assert!(dispatchable_issue_comment(
            issue_comment("created", "lassemand", false),
            &watched()
        )
        .is_none());
    }

    #[test]
    fn edited_comment_is_ignored() {
        assert!(
            dispatchable_issue_comment(issue_comment("edited", "lassemand", true), &watched())
                .is_none()
        );
    }

    #[test]
    fn comment_from_unwatched_user_is_ignored() {
        assert!(dispatchable_issue_comment(
            issue_comment("created", "somebody-else", true),
            &watched()
        )
        .is_none());
    }

    #[test]
    fn watched_user_match_is_case_insensitive() {
        assert!(dispatchable_issue_comment(
            issue_comment("created", "LasseMand", true),
            &watched()
        )
        .is_some());
        assert!(is_watched_user(&watched(), "LASSEMAND"));
        assert!(!is_watched_user(&watched(), "lassemandX"));
    }

    #[test]
    fn review_comment_carries_file_path() {
        let payload: ReviewCommentPayload = serde_json::from_value(json!({
            "action": "created",
            "pull_request": { "number": 151 },
            "comment": {
                "id": 99, "body": "why?", "user": { "login": "lassemand" },
                "path": "infra/terraform/tmux/versions.tf"
            },
            "repository": { "full_name": "lassemand/nexus" }
        }))
        .expect("valid payload");
        let got = dispatchable_review_comment(payload, &watched()).expect("dispatchable");
        assert_eq!(got.pr_number, 151);
        assert_eq!(
            got.file_path.as_deref(),
            Some("infra/terraform/tmux/versions.tf")
        );
    }

    /// Known vector: HMAC-SHA256 of the body under the secret, checked against a
    /// hand-computed value so the test fails if the algorithm or encoding changes.
    #[test]
    fn signature_valid_invalid_and_missing() {
        let secret = "s3cret";
        let body = br#"{"action":"created"}"#;

        let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).expect("key");
        mac.update(body);
        let expected = hex::encode(mac.finalize().into_bytes());
        let sig = format!("sha256={expected}");

        // Valid.
        assert!(verify_github_signature(secret, &sig, body));
        // Invalid: tampered body, wrong secret, wrong digest.
        assert!(!verify_github_signature(secret, &sig, b"tampered"));
        assert!(!verify_github_signature("wrong-secret", &sig, body));
        assert!(!verify_github_signature(
            secret,
            &format!("sha256={}", "0".repeat(64)),
            body
        ));
        // Missing / malformed: no header, no sha256= prefix, undecodable hex.
        assert!(!verify_github_signature(secret, "", body));
        assert!(!verify_github_signature(secret, &expected, body));
        assert!(!verify_github_signature(secret, "sha256=zzzz", body));
    }

    fn comment_with_body(login: &str, body: &str) -> IssueCommentPayload {
        serde_json::from_value(json!({
            "action": "created",
            "issue": { "number": 316, "pull_request": { "url": "u" } },
            "comment": { "id": 5, "body": body, "user": { "login": login } },
            "repository": { "full_name": "lassemand/nexus" }
        }))
        .expect("valid payload")
    }

    #[test]
    fn review_commands_are_recognised() {
        for yes in ["/review", "  /REVIEW  ", "/Review please", "/review\nnow"] {
            assert!(is_review_command(yes), "{yes:?}");
        }
        for no in ["/reviewer", "please /review", "review", "", "/revie"] {
            assert!(!is_review_command(no), "{no:?}");
        }
    }

    #[test]
    fn review_command_from_a_watched_user_on_a_pr() {
        let got = review_command(&comment_with_body("lassemand", "/review"), &watched())
            .expect("a review command");
        assert_eq!(got.pr_number, 316);
        assert_eq!(got.comment_id, Some(5));
        assert_eq!(got.repo, "lassemand/nexus");
    }

    #[test]
    fn review_command_from_an_unwatched_user_is_ignored() {
        assert!(review_command(&comment_with_body("stranger", "/review"), &watched()).is_none());
        assert!(
            review_command(&comment_with_body("lassemand", "looks good"), &watched()).is_none()
        );
    }

    #[test]
    fn the_review_bot_is_recognised_case_insensitively() {
        assert!(is_review_bot(
            Some("annette-review[bot]"),
            "Annette-Review[bot]"
        ));
        assert!(!is_review_bot(Some("annette-review[bot]"), "lassemand"));
        assert!(!is_review_bot(None, "annette-review[bot]"));
        assert!(!is_review_bot(Some(""), ""));
    }

    fn pr_event(action: &str, reviewer: &str, sender: &str) -> PullRequestEventPayload {
        serde_json::from_value(json!({
            "action": action,
            "number": 318,
            "requested_reviewer": { "login": reviewer, "type": "Bot" },
            "sender": { "login": sender },
            "repository": { "full_name": "lassemand/nexus" }
        }))
        .expect("valid payload")
    }

    #[test]
    fn requesting_the_reviewer_app_is_a_review_request() {
        let bot = Some("annette-review[bot]");
        let got = review_request(
            &pr_event("review_requested", "annette-review[bot]", "lassemand"),
            &watched(),
            bot,
        )
        .expect("a review request");
        assert_eq!(got.pr_number, 318);
        assert_eq!(got.comment_id, None);
        assert_eq!(got.login, "lassemand");
        assert!(review_request(
            &pr_event("review_requested", "someone", "lassemand"),
            &watched(),
            bot
        )
        .is_none());
        assert!(review_request(
            &pr_event("review_requested", "annette-review[bot]", "stranger"),
            &watched(),
            bot
        )
        .is_none());
        assert!(review_request(
            &pr_event("review_request_removed", "annette-review[bot]", "lassemand"),
            &watched(),
            bot
        )
        .is_none());
        assert!(review_request(
            &pr_event("review_requested", "annette-review[bot]", "lassemand"),
            &watched(),
            None
        )
        .is_none());
    }
}
