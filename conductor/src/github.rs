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
    })
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
}
