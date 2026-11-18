//! GitHub REST calls the review orchestration makes: reading a pull request,
//! labelling it, reacting to and answering a `/review` comment.
//!
//! Authenticates as the author app conductor already runs as: `GITHUB_TOKEN`
//! when it is set, otherwise an installation token minted by the GitHub App
//! credential helper the entrypoint installs for git, cached for less than its
//! hour.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use serde::Deserialize;
use serde_json::json;
use tokio::sync::Mutex;

use crate::review::ci_gate::CheckRun;
use crate::review::{PullRequest, ReviewGithub};

/// How long a minted installation token is reused; GitHub issues them for an hour.
const APP_TOKEN_TTL: Duration = Duration::from_secs(50 * 60);

const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// Why a GitHub call failed.
#[derive(Debug, thiserror::Error)]
pub enum GithubApiError {
    #[error("request to GitHub failed: {0}")]
    Network(String),
    #[error("GitHub answered HTTP {0}")]
    Status(u16),
    #[error("malformed GitHub response: {0}")]
    Malformed(String),
    #[error("no GitHub credentials: {0}")]
    NoToken(String),
}

/// Where the GitHub token comes from.
pub enum GithubToken {
    /// A fixed token, `GITHUB_TOKEN`.
    Static(String),
    /// The GitHub App credential helper (`get` on stdin, `password=` on stdout).
    AppHelper {
        helper: PathBuf,
        cached: Mutex<Option<(String, Instant)>>,
    },
}

impl std::fmt::Debug for GithubToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            GithubToken::Static(_) => f.write_str("GithubToken::Static(<redacted>)"),
            GithubToken::AppHelper { helper, .. } => f
                .debug_struct("GithubToken::AppHelper")
                .field("helper", helper)
                .finish(),
        }
    }
}

impl GithubToken {
    /// `GITHUB_TOKEN` if set, else the credential helper named by
    /// `GITHUB_APP_CREDENTIAL_HELPER`, else `None`.
    pub fn from_env(github_token: Option<String>) -> Option<Self> {
        if let Some(token) = github_token.filter(|t| !t.is_empty()) {
            return Some(GithubToken::Static(token));
        }
        std::env::var("GITHUB_APP_CREDENTIAL_HELPER")
            .ok()
            .filter(|h| !h.is_empty())
            .map(|helper| GithubToken::AppHelper {
                helper: PathBuf::from(helper),
                cached: Mutex::new(None),
            })
    }

    async fn get(&self) -> Result<String, GithubApiError> {
        match self {
            GithubToken::Static(token) => Ok(token.clone()),
            GithubToken::AppHelper { helper, cached } => {
                let mut cached = cached.lock().await;
                if let Some((token, at)) = cached.as_ref() {
                    if at.elapsed() < APP_TOKEN_TTL {
                        return Ok(token.clone());
                    }
                }
                let token = mint(helper).await?;
                *cached = Some((token.clone(), Instant::now()));
                Ok(token)
            }
        }
    }
}

/// Runs the credential helper and reads the token from its `password=` line.
async fn mint(helper: &PathBuf) -> Result<String, GithubApiError> {
    use tokio::io::AsyncWriteExt;
    let mut child = tokio::process::Command::new(helper)
        .arg("get")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map_err(|e| GithubApiError::NoToken(format!("cannot run the credential helper: {e}")))?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin
            .write_all(b"protocol=https\nhost=github.com\n\n")
            .await
            .map_err(|e| GithubApiError::NoToken(e.to_string()))?;
    }
    let out = child
        .wait_with_output()
        .await
        .map_err(|e| GithubApiError::NoToken(e.to_string()))?;
    if !out.status.success() {
        return Err(GithubApiError::NoToken(
            "the credential helper failed".into(),
        ));
    }
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .find_map(|l| l.strip_prefix("password=").map(str::to_string))
        .filter(|t| !t.is_empty())
        .ok_or_else(|| GithubApiError::NoToken("the credential helper returned no token".into()))
}

#[derive(Deserialize)]
struct PrBody {
    number: u64,
    html_url: String,
    state: String,
    #[serde(default)]
    merged: bool,
    body: Option<String>,
    head: PrHead,
    base: Option<PrBase>,
    user: Option<PrUser>,
}

#[derive(Deserialize)]
struct PrUser {
    login: String,
}

#[derive(Deserialize)]
struct PrHead {
    sha: String,
    #[serde(rename = "ref")]
    ref_name: String,
    repo: Option<PrRepo>,
}

#[derive(Deserialize)]
struct PrBase {
    repo: Option<PrRepo>,
}

#[derive(Deserialize)]
struct PrRepo {
    full_name: Option<String>,
}

#[derive(Deserialize)]
struct CheckRunsPage {
    #[serde(default)]
    total_count: usize,
    #[serde(default)]
    check_runs: Vec<CheckRunBody>,
}

#[derive(Deserialize)]
struct CheckRunBody {
    name: String,
    status: String,
    conclusion: Option<String>,
    html_url: Option<String>,
    details_url: Option<String>,
}

/// Check runs asked for per page, GitHub's maximum.
const CHECK_RUNS_PER_PAGE: usize = 100;

/// Pages read before the list is given up as malformed.
const CHECK_RUNS_MAX_PAGES: usize = 30;

/// [`ReviewGithub`] over GitHub's REST API.
pub struct GithubRest {
    http: reqwest::Client,
    api_url: String,
    token: GithubToken,
}

impl GithubRest {
    /// Builds a client against `api_url` (`https://api.github.com` in production).
    pub fn new(api_url: &str, token: GithubToken) -> Result<Self, GithubApiError> {
        Self::with_timeout(api_url, token, REQUEST_TIMEOUT)
    }

    /// As [`GithubRest::new`], with an explicit request timeout.
    pub fn with_timeout(
        api_url: &str,
        token: GithubToken,
        timeout: Duration,
    ) -> Result<Self, GithubApiError> {
        let http = reqwest::Client::builder()
            .timeout(timeout)
            .user_agent("nexus-conductor")
            .build()
            .map_err(|e| GithubApiError::Network(e.to_string()))?;
        Ok(Self {
            http,
            api_url: api_url.trim_end_matches('/').to_string(),
            token,
        })
    }

    async fn send(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Option<serde_json::Value>,
    ) -> Result<reqwest::Response, GithubApiError> {
        let token = self.token.get().await?;
        let mut request = self
            .http
            .request(method, format!("{}{path}", self.api_url))
            .header("Authorization", format!("Bearer {token}"))
            .header("Accept", "application/vnd.github+json");
        if let Some(body) = body {
            request = request.json(&body);
        }
        request.send().await.map_err(|e| {
            metrics::counter!("conductor_github_api_errors_total").increment(1);
            GithubApiError::Network(e.without_url().to_string())
        })
    }

    fn check(response: &reqwest::Response, allow: &[u16]) -> Result<(), GithubApiError> {
        let status = response.status().as_u16();
        if response.status().is_success() || allow.contains(&status) {
            return Ok(());
        }
        metrics::counter!("conductor_github_api_errors_total").increment(1);
        Err(GithubApiError::Status(status))
    }
}

#[async_trait::async_trait]
impl ReviewGithub for GithubRest {
    async fn pull_request(&self, repo: &str, number: u64) -> Result<PullRequest, GithubApiError> {
        let response = self
            .send(
                reqwest::Method::GET,
                &format!("/repos/{repo}/pulls/{number}"),
                None,
            )
            .await?;
        Self::check(&response, &[])?;
        let pr: PrBody = response
            .json()
            .await
            .map_err(|e| GithubApiError::Malformed(e.without_url().to_string()))?;
        Ok(PullRequest {
            repo: repo.to_string(),
            number: pr.number,
            url: pr.html_url,
            open: pr.state == "open" && !pr.merged,
            head_sha: pr.head.sha,
            head_ref: pr.head.ref_name,
            head_repo: pr.head.repo.and_then(|r| r.full_name),
            base_repo: pr.base.and_then(|b| b.repo).and_then(|r| r.full_name),
            body: pr.body.unwrap_or_default(),
            author: pr.user.map(|u| u.login).unwrap_or_default(),
        })
    }

    async fn add_label(&self, repo: &str, number: u64, label: &str) -> Result<(), GithubApiError> {
        let response = self
            .send(
                reqwest::Method::POST,
                &format!("/repos/{repo}/issues/{number}/labels"),
                Some(json!({ "labels": [label] })),
            )
            .await?;
        Self::check(&response, &[])
    }

    async fn remove_label(
        &self,
        repo: &str,
        number: u64,
        label: &str,
    ) -> Result<(), GithubApiError> {
        let response = self
            .send(
                reqwest::Method::DELETE,
                &format!("/repos/{repo}/issues/{number}/labels/{label}"),
                None,
            )
            .await?;
        Self::check(&response, &[404])
    }

    async fn react(
        &self,
        repo: &str,
        comment_id: u64,
        content: &str,
    ) -> Result<(), GithubApiError> {
        let response = self
            .send(
                reqwest::Method::POST,
                &format!("/repos/{repo}/issues/comments/{comment_id}/reactions"),
                Some(json!({ "content": content })),
            )
            .await?;
        Self::check(&response, &[])
    }

    async fn comment(&self, repo: &str, number: u64, body: &str) -> Result<(), GithubApiError> {
        let response = self
            .send(
                reqwest::Method::POST,
                &format!("/repos/{repo}/issues/{number}/comments"),
                Some(json!({ "body": body })),
            )
            .await?;
        Self::check(&response, &[])
    }

    async fn check_runs(&self, repo: &str, sha: &str) -> Result<Vec<CheckRun>, GithubApiError> {
        let mut out: Vec<CheckRun> = Vec::new();
        for page in 1..=CHECK_RUNS_MAX_PAGES {
            let response = self
                .send(
                    reqwest::Method::GET,
                    &format!(
                        "/repos/{repo}/commits/{sha}/check-runs?per_page={CHECK_RUNS_PER_PAGE}&page={page}"
                    ),
                    None,
                )
                .await?;
            Self::check(&response, &[])?;
            let body: CheckRunsPage = response
                .json()
                .await
                .map_err(|e| GithubApiError::Malformed(e.without_url().to_string()))?;
            let got = body.check_runs.len();
            out.extend(body.check_runs.into_iter().map(|c| CheckRun {
                name: c.name,
                status: c.status,
                conclusion: c.conclusion,
                url: c.html_url.or(c.details_url).unwrap_or_default(),
            }));
            if got < CHECK_RUNS_PER_PAGE || out.len() >= body.total_count {
                return Ok(out);
            }
        }
        Err(GithubApiError::Malformed(
            "check runs did not end after the page limit".into(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{body_json, header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn client(server: &MockServer) -> GithubRest {
        GithubRest::new(&server.uri(), GithubToken::Static("ghs_x".into())).unwrap()
    }

    #[tokio::test]
    async fn reads_a_pull_request() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/repos/o/r/pulls/7"))
            .and(header("Authorization", "Bearer ghs_x"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "number": 7, "html_url": "https://github.com/o/r/pull/7", "state": "open",
                "merged": false, "body": "Fixes NEX-1",
                "head": { "sha": "abc", "ref": "feature/NEX-1-x" }
            })))
            .mount(&server)
            .await;
        let pr = client(&server).pull_request("o/r", 7).await.unwrap();
        assert!(pr.open);
        assert_eq!(pr.head_sha, "abc");
        assert_eq!(pr.head_ref, "feature/NEX-1-x");
    }

    #[tokio::test]
    async fn a_merged_pull_request_is_not_open() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "number": 7, "html_url": "u", "state": "closed", "merged": true,
                "body": null, "head": { "sha": "abc", "ref": "b" }
            })))
            .mount(&server)
            .await;
        assert!(!client(&server).pull_request("o/r", 7).await.unwrap().open);
    }

    #[tokio::test]
    async fn labels_reactions_and_comments() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/repos/o/r/issues/7/labels"))
            .and(body_json(serde_json::json!({ "labels": ["needs-human"] })))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("DELETE"))
            .and(path("/repos/o/r/issues/7/labels/needs-human"))
            .respond_with(ResponseTemplate::new(404))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/repos/o/r/issues/comments/9/reactions"))
            .and(body_json(serde_json::json!({ "content": "eyes" })))
            .respond_with(ResponseTemplate::new(201))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/repos/o/r/issues/7/comments"))
            .respond_with(ResponseTemplate::new(201))
            .expect(1)
            .mount(&server)
            .await;
        let c = client(&server);
        c.add_label("o/r", 7, "needs-human").await.unwrap();
        c.remove_label("o/r", 7, "needs-human").await.unwrap();
        c.react("o/r", 9, "eyes").await.unwrap();
        c.comment("o/r", 7, "hi").await.unwrap();
    }

    #[tokio::test]
    async fn check_runs_are_read_across_pages() {
        let server = MockServer::start().await;
        let run = |n: usize| {
            serde_json::json!({
                "name": format!("c{n}"), "status": "completed", "conclusion": "success",
                "html_url": format!("https://github.com/o/r/runs/{n}")
            })
        };
        let first: Vec<_> = (0..100).map(run).collect();
        Mock::given(method("GET"))
            .and(path("/repos/o/r/commits/abc/check-runs"))
            .and(wiremock::matchers::query_param("page", "1"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({ "total_count": 101, "check_runs": first })),
            )
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/repos/o/r/commits/abc/check-runs"))
            .and(wiremock::matchers::query_param("page", "2"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(
                    serde_json::json!({ "total_count": 101, "check_runs": [run(100)] }),
                ),
            )
            .mount(&server)
            .await;
        let runs = client(&server).check_runs("o/r", "abc").await.unwrap();
        assert_eq!(runs.len(), 101);
        assert_eq!(runs[100].url, "https://github.com/o/r/runs/100");
    }

    #[tokio::test]
    async fn a_failing_check_run_read_is_an_error() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(502))
            .mount(&server)
            .await;
        assert!(client(&server).check_runs("o/r", "abc").await.is_err());
    }

    #[tokio::test]
    async fn the_credential_helper_token_is_used() {
        let dir = tempfile::tempdir().unwrap();
        let helper = dir.path().join("helper");
        let status = std::process::Command::new("sh")
            .args([
                "-c",
                r#"printf '#!/bin/sh\ncat >/dev/null\necho password=minted\n' > "$0" && chmod 755 "$0""#,
            ])
            .arg(&helper)
            .status()
            .unwrap();
        assert!(status.success());
        let token = GithubToken::AppHelper {
            helper,
            cached: Mutex::new(None),
        };
        assert_eq!(token.get().await.unwrap(), "minted");
    }
}
