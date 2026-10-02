//! Routing a pull request comment to the session that wrote the pull request.
//!
//! A review comment should reach the Claude session that produced the branch,
//! because that session already holds the context needed to act on it. The
//! dispatcher records every branch it sees in a group's worktree, so routing is
//! a lookup from the pull request's head branch back to the owning group.

use std::collections::HashMap;
use std::time::Duration;

use async_trait::async_trait;
use serde::Deserialize;
use tokio::sync::Mutex;

use crate::github::GithubPrComment;
use crate::registry::{group_key_for, GroupKey, Registry};
use crate::InboundEvent;

/// How long to wait for the pull request lookup.
const LOOKUP_TIMEOUT: Duration = Duration::from_secs(10);

/// Longest a group key may be; shared with the registry's validation.
const MAX_KEY_LEN: usize = 63;

/// Chooses the session group an event belongs to.
///
/// `None` means drop the event: it is not something this system can act on.
#[async_trait]
pub trait GroupResolver: Send + Sync + 'static {
    /// The group to dispatch into, or `None` to drop the event.
    async fn resolve(&self, event: &InboundEvent) -> Option<GroupKey>;

    /// Base ref a newly created group's worktree should start from.
    ///
    /// Defaults to `None`, meaning the usual fresh `agent/<slug>` from main, so
    /// a simpler resolver need not implement it.
    async fn worktree_ref(&self, _event: &InboundEvent) -> Option<String> {
        None
    }
}

/// What routing needs to know about a pull request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrRefs {
    /// Branch the pull request is from.
    pub head_ref: String,
    /// Repository that branch lives in, when known.
    pub head_repo: Option<String>,
    /// Repository the pull request targets, when known.
    pub base_repo: Option<String>,
}

/// Subset of GitHub's pull request representation that routing reads.
#[derive(Debug, Deserialize)]
struct PullRequestBody {
    head: Option<ApiBranch>,
    base: Option<ApiBranch>,
}

#[derive(Debug, Deserialize)]
struct ApiBranch {
    #[serde(rename = "ref")]
    ref_name: Option<String>,
    repo: Option<ApiRepo>,
}

#[derive(Debug, Deserialize)]
struct ApiRepo {
    full_name: Option<String>,
}

/// The group key used when no session owns the pull request.
///
/// The repository part is truncated rather than the whole key, so the pull
/// request number always survives — truncating the end would collapse different
/// pull requests in a long-named repository into one group.
pub fn fallback_key(repo: &str, pr_number: u64) -> GroupKey {
    const PREFIX: &str = "gh-";
    let suffix = format!("-pr-{pr_number}");

    let sanitised: String = repo
        .to_lowercase()
        .chars()
        .map(|c| {
            if c.is_ascii_lowercase() || c.is_ascii_digit() {
                c
            } else {
                '-'
            }
        })
        .collect();

    let room = MAX_KEY_LEN.saturating_sub(PREFIX.len() + suffix.len());
    let mut slug: String = sanitised.chars().take(room).collect();
    // A trailing separator reads badly and adds nothing.
    while slug.ends_with('-') {
        slug.pop();
    }

    GroupKey::Named(format!("{PREFIX}{slug}{suffix}"))
}

/// Resolves pull request comments to the group that produced the branch.
pub struct BranchResolver {
    registry: Registry,
    http: reqwest::Client,
    api_url: String,
    /// Absent in local development, where the lookup is skipped.
    token: Option<String>,
    /// A pull request's head ref does not change, so results are kept for the
    /// life of the process. Only successes are cached: caching a transient
    /// failure would misroute every later comment on that pull request.
    cache: Mutex<HashMap<(String, u64), PrRefs>>,
}

impl BranchResolver {
    /// Builds a resolver. `api_url` is overridable so tests can point at a mock.
    pub fn new(registry: Registry, api_url: String, token: Option<String>) -> Self {
        Self::with_timeout(registry, api_url, token, LOOKUP_TIMEOUT)
    }

    /// As [`BranchResolver::new`], with an explicit lookup timeout so a test need
    /// not wait the production ten seconds to exercise it.
    pub fn with_timeout(
        registry: Registry,
        api_url: String,
        token: Option<String>,
        timeout: Duration,
    ) -> Self {
        BranchResolver {
            registry,
            http: reqwest::Client::builder()
                .timeout(timeout)
                .build()
                .unwrap_or_default(),
            api_url: api_url.trim_end_matches('/').to_string(),
            token,
            cache: Mutex::new(HashMap::new()),
        }
    }

    /// The pull request's refs, from the payload if it carried them, else the API.
    async fn pr_refs(&self, comment: &GithubPrComment) -> Option<PrRefs> {
        // Review comments include the branch, so no request is needed.
        if let Some(head_ref) = &comment.head_ref {
            return Some(PrRefs {
                head_ref: head_ref.clone(),
                head_repo: comment.head_repo.clone(),
                base_repo: comment.base_repo.clone(),
            });
        }

        let cache_key = (comment.repo.clone(), comment.pr_number);
        if let Some(hit) = self.cache.lock().await.get(&cache_key) {
            return Some(hit.clone());
        }

        let Some(token) = &self.token else {
            tracing::warn!(
                repo = %comment.repo,
                pr = comment.pr_number,
                "GITHUB_TOKEN is not set; cannot find the pull request's branch"
            );
            return None;
        };

        let url = format!(
            "{}/repos/{}/pulls/{}",
            self.api_url, comment.repo, comment.pr_number
        );
        let response = self
            .http
            .get(&url)
            .header("Authorization", format!("Bearer {token}"))
            .header("Accept", "application/vnd.github+json")
            .header("User-Agent", "nexus-conductor")
            .send()
            .await;

        let body = match response {
            Ok(r) if r.status().is_success() => r.json::<PullRequestBody>().await.ok()?,
            Ok(r) => {
                tracing::warn!(
                    repo = %comment.repo,
                    pr = comment.pr_number,
                    status = %r.status(),
                    "pull request lookup failed"
                );
                return None;
            }
            Err(e) => {
                tracing::warn!(
                    repo = %comment.repo,
                    pr = comment.pr_number,
                    error = %e,
                    "pull request lookup failed"
                );
                return None;
            }
        };

        let head = body.head?;
        let refs = PrRefs {
            head_ref: head.ref_name?,
            head_repo: head.repo.and_then(|r| r.full_name),
            base_repo: body.base.and_then(|b| b.repo).and_then(|r| r.full_name),
        };
        self.cache.lock().await.insert(cache_key, refs.clone());
        Some(refs)
    }

    /// The group owning `head_ref`, preferring the most recently active when
    /// several have seen it.
    async fn owner_of(&self, head_ref: &str) -> Option<GroupKey> {
        let owners = match self.registry.groups_with_branch(head_ref).await {
            Ok(o) => o,
            Err(e) => {
                tracing::error!(error = %e, branch = head_ref, "branch lookup failed");
                return None;
            }
        };

        match owners.as_slice() {
            [] => None,
            [(key, _)] => Some(key.clone()),
            many => {
                // Ordered by last_active descending, so the first is the newest.
                tracing::warn!(
                    branch = head_ref,
                    candidates = ?many.iter().map(|(k, _)| k.slug()).collect::<Vec<_>>(),
                    "several groups have seen this branch; using the most recently active"
                );
                many.first().map(|(key, _)| key.clone())
            }
        }
    }
}

#[async_trait]
impl GroupResolver for BranchResolver {
    async fn resolve(&self, event: &InboundEvent) -> Option<GroupKey> {
        let comment = match event {
            // Linear events already carry their own key.
            InboundEvent::LinearIssueTodo { .. } => return Some(group_key_for(event)),
            InboundEvent::GithubPrComment { comment, .. } => comment,
        };

        let fallback = fallback_key(&comment.repo, comment.pr_number);

        let Some(refs) = self.pr_refs(comment).await else {
            // The branch is unknown, so ownership cannot be determined. The
            // comment still deserves an answer, in a group of its own.
            tracing::warn!(
                repo = %comment.repo,
                pr = comment.pr_number,
                group = %fallback.slug(),
                "routing to a per-pull-request group"
            );
            return Some(fallback);
        };

        // A fork's branch is not ours to push to, so there is nothing useful a
        // session could do with the comment.
        if let (Some(head), Some(base)) = (&refs.head_repo, &refs.base_repo) {
            if head != base {
                tracing::warn!(
                    repo = %comment.repo,
                    pr = comment.pr_number,
                    head_repo = %head,
                    base_repo = %base,
                    "pull request is from a fork; dropping the comment"
                );
                return None;
            }
        }

        match self.owner_of(&refs.head_ref).await {
            Some(owner) => {
                tracing::info!(
                    repo = %comment.repo,
                    pr = comment.pr_number,
                    branch = %refs.head_ref,
                    group = %owner.slug(),
                    "routing to the group that produced the branch"
                );
                Some(owner)
            }
            None => {
                tracing::info!(
                    repo = %comment.repo,
                    pr = comment.pr_number,
                    branch = %refs.head_ref,
                    group = %fallback.slug(),
                    "no session owns this branch; routing to a per-pull-request group"
                );
                Some(fallback)
            }
        }
    }

    async fn worktree_ref(&self, event: &InboundEvent) -> Option<String> {
        let InboundEvent::GithubPrComment { comment, .. } = event else {
            return None;
        };
        // Cached by the preceding resolve, so this costs nothing.
        self.pr_refs(comment).await.map(|r| r.head_ref)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::github::PrCommentKind;
    use crate::registry::tests_support::{epoch, FakeClock};
    use crate::registry::{GroupKey, DEFAULT_IDLE_DAYS};
    use serde_json::json;
    use sqlx::PgPool;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    /// A review comment, which carries the branch in its payload.
    fn review_comment(repo: &str, pr: u64, head: &str, head_repo: &str) -> InboundEvent {
        InboundEvent::GithubPrComment {
            comment: GithubPrComment {
                repo: repo.into(),
                pr_number: pr,
                comment_id: 1,
                body: "why?".into(),
                file_path: Some("a.rs".into()),
                head_ref: Some(head.into()),
                head_repo: Some(head_repo.into()),
                base_repo: Some(repo.into()),
            },
            kind: PrCommentKind::ReviewComment,
            delivery_id: None,
        }
    }

    /// An issue comment, which does not: the branch has to be fetched.
    fn issue_comment(repo: &str, pr: u64) -> InboundEvent {
        InboundEvent::GithubPrComment {
            comment: GithubPrComment {
                repo: repo.into(),
                pr_number: pr,
                comment_id: 2,
                body: "ping".into(),
                file_path: None,
                head_ref: None,
                head_repo: None,
                base_repo: None,
            },
            kind: PrCommentKind::Comment,
            delivery_id: None,
        }
    }

    /// Creates a group and records `branch` as seen in its worktree.
    async fn own_branch(registry: &Registry, slug: &str, branch: &str) -> GroupKey {
        let key = GroupKey::Named(slug.into());
        let mut txn = registry.begin().await.expect("begin");
        txn.session_for_dispatch(&key).await.expect("create");
        txn.record_branch(&key, branch).await.expect("branch");
        txn.commit().await.expect("commit");
        key
    }

    fn pr_body(head: &str, head_repo: &str, base_repo: &str) -> serde_json::Value {
        json!({
            "head": { "ref": head, "repo": { "full_name": head_repo } },
            "base": { "ref": "main", "repo": { "full_name": base_repo } }
        })
    }

    // ── key shape (no database) ──────────────────────────────────────────────

    #[test]
    fn fallback_key_is_predictable_and_safe() {
        assert_eq!(
            fallback_key("lassemand/nexus", 153),
            GroupKey::Named("gh-lassemand-nexus-pr-153".into())
        );
        // Characters that cannot appear in a path or branch are replaced.
        let GroupKey::Named(name) = fallback_key("Weird.Org/Repo_Name", 7) else {
            panic!("named");
        };
        assert!(name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-'));
    }

    /// The pull request number must survive truncation, or two pull requests in a
    /// long-named repository would collapse into one group.
    #[test]
    fn a_very_long_repo_name_keeps_the_pr_number_and_stays_valid() {
        let repo = format!("{}/{}", "o".repeat(80), "r".repeat(80));
        let GroupKey::Named(name) = fallback_key(&repo, 4242) else {
            panic!("named");
        };
        assert!(name.len() <= 63, "key is {} chars: {name}", name.len());
        assert!(name.ends_with("-pr-4242"), "lost the pr number: {name}");
        assert!(name.starts_with("gh-"));
        // Still a legal key: lowercase alphanumeric and dashes, no leading dash.
        assert!(name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-'));
        assert!(!name.starts_with('-'));

        // And two pull requests in that repo stay distinct.
        assert_ne!(fallback_key(&repo, 1), fallback_key(&repo, 2));
    }

    // ── routing (needs Postgres) ─────────────────────────────────────────────

    #[sqlx::test(migrations = "./migrations")]
    async fn review_comment_routes_by_payload_without_calling_the_api(pool: PgPool) {
        let server = MockServer::start().await;
        let registry = Registry::new(pool, FakeClock::new(epoch()), DEFAULT_IDLE_DAYS);
        let owner = own_branch(&registry, "group-a", "feature/x").await;

        let resolver = BranchResolver::new(registry, server.uri(), Some("t".into()));
        let key = resolver
            .resolve(&review_comment("o/r", 5, "feature/x", "o/r"))
            .await;

        assert_eq!(key, Some(owner));
        assert!(
            server
                .received_requests()
                .await
                .unwrap_or_default()
                .is_empty(),
            "the payload already had the branch; no request should be made"
        );
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn issue_comment_fetches_once_and_caches(pool: PgPool) {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/repos/o/r/pulls/9"))
            .respond_with(ResponseTemplate::new(200).set_body_json(pr_body(
                "feature/y",
                "o/r",
                "o/r",
            )))
            .mount(&server)
            .await;

        let registry = Registry::new(pool, FakeClock::new(epoch()), DEFAULT_IDLE_DAYS);
        let owner = own_branch(&registry, "group-a", "feature/y").await;
        let resolver = BranchResolver::new(registry, server.uri(), Some("t".into()));

        assert_eq!(
            resolver.resolve(&issue_comment("o/r", 9)).await,
            Some(owner.clone())
        );
        assert_eq!(
            server.received_requests().await.unwrap_or_default().len(),
            1
        );

        // A pull request's head ref does not change, so a second comment is free.
        assert_eq!(
            resolver.resolve(&issue_comment("o/r", 9)).await,
            Some(owner)
        );
        assert_eq!(
            server.received_requests().await.unwrap_or_default().len(),
            1,
            "the second lookup should have been served from cache"
        );
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn the_most_recently_active_owner_wins(pool: PgPool) {
        let server = MockServer::start().await;
        let clock = FakeClock::new(epoch());
        let registry = Registry::new(pool, clock.clone(), DEFAULT_IDLE_DAYS);

        own_branch(&registry, "group-a", "feature/shared").await;
        // B touches the branch later, so it is the better place to send a review.
        clock.advance(chrono::Duration::hours(1));
        let newer = own_branch(&registry, "group-b", "feature/shared").await;

        let resolver = BranchResolver::new(registry, server.uri(), Some("t".into()));
        let key = resolver
            .resolve(&review_comment("o/r", 5, "feature/shared", "o/r"))
            .await;

        assert_eq!(key, Some(newer));
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn an_unowned_branch_gets_its_own_group(pool: PgPool) {
        let server = MockServer::start().await;
        let registry = Registry::new(pool, FakeClock::new(epoch()), DEFAULT_IDLE_DAYS);
        let resolver = BranchResolver::new(registry, server.uri(), Some("t".into()));

        let key = resolver
            .resolve(&review_comment("o/r", 11, "somebodys-branch", "o/r"))
            .await;

        assert_eq!(key, Some(fallback_key("o/r", 11)));
        // And that group should start on the pull request's branch.
        let base = resolver
            .worktree_ref(&review_comment("o/r", 11, "somebodys-branch", "o/r"))
            .await;
        assert_eq!(base.as_deref(), Some("somebodys-branch"));
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn a_fork_pull_request_is_dropped(pool: PgPool) {
        let server = MockServer::start().await;
        let registry = Registry::new(pool, FakeClock::new(epoch()), DEFAULT_IDLE_DAYS);
        // Even if a group has seen a branch of that name, a fork is not ours to push to.
        own_branch(&registry, "group-a", "feature/x").await;
        let resolver = BranchResolver::new(registry, server.uri(), Some("t".into()));

        let key = resolver
            .resolve(&review_comment("o/r", 5, "feature/x", "someone-else/r"))
            .await;

        assert_eq!(key, None, "a fork's branch cannot be pushed to");
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn api_failures_fall_back_rather_than_dropping(pool: PgPool) {
        for status in [404u16, 500] {
            let server = MockServer::start().await;
            Mock::given(method("GET"))
                .respond_with(ResponseTemplate::new(status))
                .mount(&server)
                .await;

            let registry = Registry::new(pool.clone(), FakeClock::new(epoch()), DEFAULT_IDLE_DAYS);
            let resolver = BranchResolver::new(registry, server.uri(), Some("t".into()));

            assert_eq!(
                resolver.resolve(&issue_comment("o/r", 3)).await,
                Some(fallback_key("o/r", 3)),
                "status {status} should fall back, not drop the comment"
            );
        }
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn a_lookup_timeout_falls_back(pool: PgPool) {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(pr_body("feature/y", "o/r", "o/r"))
                    .set_delay(Duration::from_millis(500)),
            )
            .mount(&server)
            .await;

        let registry = Registry::new(pool, FakeClock::new(epoch()), DEFAULT_IDLE_DAYS);
        // Far below the response delay, so the request cannot complete.
        let resolver = BranchResolver::with_timeout(
            registry,
            server.uri(),
            Some("t".into()),
            Duration::from_millis(50),
        );

        assert_eq!(
            resolver.resolve(&issue_comment("o/r", 3)).await,
            Some(fallback_key("o/r", 3))
        );
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn without_a_token_the_lookup_is_skipped(pool: PgPool) {
        let server = MockServer::start().await;
        let registry = Registry::new(pool, FakeClock::new(epoch()), DEFAULT_IDLE_DAYS);
        let resolver = BranchResolver::new(registry, server.uri(), None);

        assert_eq!(
            resolver.resolve(&issue_comment("o/r", 7)).await,
            Some(fallback_key("o/r", 7))
        );
        assert!(
            server
                .received_requests()
                .await
                .unwrap_or_default()
                .is_empty(),
            "no token means no request"
        );
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn linear_events_are_routed_by_label_not_by_branch(pool: PgPool) {
        let server = MockServer::start().await;
        let registry = Registry::new(pool, FakeClock::new(epoch()), DEFAULT_IDLE_DAYS);
        let resolver = BranchResolver::new(registry, server.uri(), Some("t".into()));

        let event = crate::registry::tests_support::linear_event("NEX-42", &["conductor-core"]);
        assert_eq!(
            resolver.resolve(&event).await,
            Some(GroupKey::Named("conductor-core".into()))
        );
        assert!(server
            .received_requests()
            .await
            .unwrap_or_default()
            .is_empty());
    }
}
