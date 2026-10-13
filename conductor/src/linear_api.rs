//! Linear GraphQL client that lists every issue in a session set.
//!
//! Every failure is an `Err`, never an empty `Ok`: callers use the answer to
//! decide whether a session may be closed, so an uncertain answer must never
//! look like "the set is empty".

use std::time::Duration;

use serde::Deserialize;
use serde_json::{json, Value};

use crate::registry::GroupKey;

/// Default GraphQL endpoint; `CONDUCTOR_LINEAR_API_URL` overrides it.
pub const DEFAULT_API_URL: &str = "https://api.linear.app/graphql";

/// Maximum pages followed for one set before giving up.
pub const MAX_PAGES: usize = 20;

const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// Label prefix selecting a session group.
const SESSION_LABEL_PREFIX: &str = "session:";

/// `includeArchived` matters: Linear auto-archives old completed issues, and
/// without it a fully finished set could come back empty and never close.
const SET_QUERY: &str = "query SetIssues($label: String!, $cursor: String) { \
issues(filter: { labels: { name: { eqIgnoreCase: $label } } }, includeArchived: true, first: 100, after: $cursor) { \
nodes { identifier state { type } trashed } \
pageInfo { hasNextPage endCursor } } }";

const ISSUE_QUERY: &str =
    "query OneIssue($id: String!) { issue(id: $id) { identifier state { type } trashed } }";

/// One issue of a set.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SetIssue {
    pub identifier: String,
    /// Workflow state type, e.g. `completed`.
    pub state_type: String,
}

/// Why the set could not be determined.
#[derive(Debug, thiserror::Error)]
pub enum LinearApiError {
    #[error("request to Linear failed: {0}")]
    Network(String),
    #[error("Linear answered HTTP {0}")]
    Status(u16),
    #[error("Linear returned GraphQL errors: {0}")]
    GraphQl(String),
    #[error("malformed Linear response: {0}")]
    Malformed(String),
    #[error("set exceeds {MAX_PAGES} pages")]
    TooLarge,
    #[error("group key does not identify a set")]
    NotASet,
}

/// Source of the full issue list of a session group.
#[async_trait::async_trait]
pub trait IssueSetSource: Send + Sync {
    /// Lists every non-trashed issue in the group, or errors if unsure.
    async fn issues_in_set(&self, key: &GroupKey) -> Result<Vec<SetIssue>, LinearApiError>;
}

/// [`IssueSetSource`] backed by Linear's GraphQL API.
pub struct LinearGraphql {
    http: reqwest::Client,
    url: String,
    api_key: String,
}

impl std::fmt::Debug for LinearGraphql {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LinearGraphql")
            .field("url", &self.url)
            .field("api_key", &"<redacted>")
            .finish()
    }
}

#[derive(Deserialize)]
struct Envelope<T> {
    data: Option<T>,
    errors: Option<Vec<Value>>,
}

#[derive(Deserialize)]
struct Node {
    identifier: String,
    state: NodeState,
    #[serde(default)]
    trashed: bool,
}

#[derive(Deserialize)]
struct NodeState {
    #[serde(rename = "type")]
    state_type: String,
}

#[derive(Deserialize)]
struct SetData {
    issues: IssueConnection,
}

#[derive(Deserialize)]
struct IssueConnection {
    nodes: Vec<Node>,
    #[serde(rename = "pageInfo")]
    page_info: PageInfo,
}

#[derive(Deserialize)]
struct PageInfo {
    #[serde(rename = "hasNextPage")]
    has_next_page: bool,
    #[serde(rename = "endCursor")]
    end_cursor: Option<String>,
}

#[derive(Deserialize)]
struct IssueData {
    issue: Option<Node>,
}

impl Node {
    fn into_set_issue(self) -> Option<SetIssue> {
        (!self.trashed).then_some(SetIssue {
            identifier: self.identifier,
            state_type: self.state.state_type,
        })
    }
}

impl LinearGraphql {
    /// Builds a client. `api_key` is a personal API key, sent without `Bearer`.
    pub fn new(api_key: impl Into<String>, url: Option<String>) -> Result<Self, LinearApiError> {
        let http = reqwest::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .build()
            .map_err(|e| LinearApiError::Network(e.to_string()))?;
        Ok(Self {
            http,
            url: url.unwrap_or_else(|| DEFAULT_API_URL.to_string()),
            api_key: api_key.into(),
        })
    }

    /// Posts one GraphQL request and returns the parsed envelope.
    async fn post<T: for<'de> Deserialize<'de>>(
        &self,
        query: &str,
        variables: Value,
    ) -> Result<Envelope<T>, LinearApiError> {
        let response = self
            .http
            .post(&self.url)
            .header("Authorization", &self.api_key)
            .json(&json!({ "query": query, "variables": variables }))
            // reqwest's error Display can include the URL but never headers.
            .send()
            .await
            .map_err(|e| LinearApiError::Network(e.without_url().to_string()))?;
        let status = response.status();
        if status.as_u16() != 200 {
            return Err(LinearApiError::Status(status.as_u16()));
        }
        let bytes = response
            .bytes()
            .await
            .map_err(|e| LinearApiError::Network(e.without_url().to_string()))?;
        serde_json::from_slice(&bytes).map_err(|e| LinearApiError::Malformed(e.to_string()))
    }

    async fn named_set(&self, group: &str) -> Result<Vec<SetIssue>, LinearApiError> {
        let label = format!("{SESSION_LABEL_PREFIX}{group}");
        let mut out = Vec::new();
        let mut cursor: Option<String> = None;
        for _ in 0..MAX_PAGES {
            let env: Envelope<SetData> = self
                .post(SET_QUERY, json!({ "label": label, "cursor": cursor }))
                .await?;
            if let Some(errors) = env.errors.filter(|e| !e.is_empty()) {
                return Err(LinearApiError::GraphQl(Value::Array(errors).to_string()));
            }
            let page = env
                .data
                .ok_or_else(|| LinearApiError::Malformed("missing data".into()))?
                .issues;
            out.extend(page.nodes.into_iter().filter_map(Node::into_set_issue));
            if !page.page_info.has_next_page {
                return Ok(out);
            }
            // A next page without a cursor would refetch page one forever.
            cursor = Some(page.page_info.end_cursor.ok_or_else(|| {
                LinearApiError::Malformed("hasNextPage without endCursor".into())
            })?);
        }
        Err(LinearApiError::TooLarge)
    }

    async fn single(&self, identifier: &str) -> Result<Vec<SetIssue>, LinearApiError> {
        let env: Envelope<IssueData> = self.post(ISSUE_QUERY, json!({ "id": identifier })).await?;
        if let Some(errors) = env.errors.filter(|e| !e.is_empty()) {
            // Linear reports a missing issue as an error; that means deleted.
            let not_found = errors.iter().all(|e| {
                e.get("message")
                    .and_then(Value::as_str)
                    .is_some_and(|m| m.contains("Entity not found"))
            });
            if not_found {
                return Ok(Vec::new());
            }
            return Err(LinearApiError::GraphQl(Value::Array(errors).to_string()));
        }
        let data = env
            .data
            .ok_or_else(|| LinearApiError::Malformed("missing data".into()))?;
        Ok(data
            .issue
            .and_then(Node::into_set_issue)
            .into_iter()
            .collect())
    }
}

#[async_trait::async_trait]
impl IssueSetSource for LinearGraphql {
    async fn issues_in_set(&self, key: &GroupKey) -> Result<Vec<SetIssue>, LinearApiError> {
        match key {
            GroupKey::Named(group) => self.named_set(group).await,
            GroupKey::Issue(id) => self.single(id).await,
            GroupKey::Unresolved => Err(LinearApiError::NotASet),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{header, method};
    use wiremock::{Mock, MockServer, Request, ResponseTemplate};

    const KEY: &str = "lin_api_secret";

    fn client(server: &MockServer) -> LinearGraphql {
        LinearGraphql::new(KEY, Some(server.uri())).unwrap()
    }

    fn page(ids: &[(&str, &str, bool)], next: Option<&str>) -> ResponseTemplate {
        let nodes: Vec<_> = ids
            .iter()
            .map(|(i, t, tr)| json!({ "identifier": i, "state": { "type": t }, "trashed": tr }))
            .collect();
        ResponseTemplate::new(200).set_body_json(json!({ "data": { "issues": {
            "nodes": nodes,
            "pageInfo": { "hasNextPage": next.is_some(), "endCursor": next }
        }}}))
    }

    fn named(g: &str) -> GroupKey {
        GroupKey::Named(g.into())
    }

    #[tokio::test]
    async fn single_page_sends_raw_key_and_ilike_label_filter() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(header("Authorization", KEY))
            .respond_with(page(&[("NEX-1", "completed", false)], None))
            .expect(1)
            .mount(&server)
            .await;
        let got = client(&server).issues_in_set(&named("grp")).await.unwrap();
        assert_eq!(
            got,
            vec![SetIssue {
                identifier: "NEX-1".into(),
                state_type: "completed".into()
            }]
        );
        let req = &server.received_requests().await.unwrap()[0];
        let auth = req.headers.get("authorization").unwrap().to_str().unwrap();
        assert_eq!(auth, KEY);
        assert!(!auth.starts_with("Bearer"));
        let body: Value = serde_json::from_slice(&req.body).unwrap();
        let q = body["query"].as_str().unwrap();
        assert!(q.contains("eqIgnoreCase: $label"));
        assert!(q.contains("includeArchived: true"));
        assert_eq!(body["variables"]["label"], "session:grp");
    }

    #[tokio::test]
    async fn follows_cursors_across_three_pages() {
        let server = MockServer::start().await;
        let responder = |req: &Request| {
            let body: Value = serde_json::from_slice(&req.body).unwrap();
            match body["variables"]["cursor"].as_str() {
                None => page(&[("NEX-1", "completed", false)], Some("c1")),
                Some("c1") => page(&[("NEX-2", "started", false)], Some("c2")),
                _ => page(&[("NEX-3", "canceled", false)], None),
            }
        };
        Mock::given(method("POST"))
            .respond_with(responder)
            .mount(&server)
            .await;
        let got = client(&server).issues_in_set(&named("g")).await.unwrap();
        let ids: Vec<_> = got.iter().map(|i| i.identifier.as_str()).collect();
        assert_eq!(ids, ["NEX-1", "NEX-2", "NEX-3"]);
    }

    #[tokio::test]
    async fn trashed_dropped_and_archived_completed_kept() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(page(
                &[("NEX-1", "completed", false), ("NEX-2", "started", true)],
                None,
            ))
            .mount(&server)
            .await;
        let got = client(&server).issues_in_set(&named("g")).await.unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].identifier, "NEX-1");
    }

    #[tokio::test]
    async fn http_failures_are_errors() {
        for status in [401u16, 429, 500] {
            let server = MockServer::start().await;
            Mock::given(method("POST"))
                .respond_with(ResponseTemplate::new(status))
                .mount(&server)
                .await;
            let err = client(&server)
                .issues_in_set(&named("g"))
                .await
                .unwrap_err();
            assert!(matches!(err, LinearApiError::Status(s) if s == status));
        }
    }

    #[tokio::test]
    async fn graphql_errors_with_200_are_errors() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({ "data": null, "errors": [{ "message": "boom" }] })),
            )
            .mount(&server)
            .await;
        let err = client(&server)
            .issues_in_set(&named("g"))
            .await
            .unwrap_err();
        assert!(matches!(err, LinearApiError::GraphQl(_)));
    }

    #[tokio::test]
    async fn malformed_json_is_an_error() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_string("not json"))
            .mount(&server)
            .await;
        let err = client(&server)
            .issues_in_set(&named("g"))
            .await
            .unwrap_err();
        assert!(matches!(err, LinearApiError::Malformed(_)));
    }

    #[tokio::test]
    async fn more_than_max_pages_is_too_large() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(page(&[("NEX-1", "completed", false)], Some("again")))
            .mount(&server)
            .await;
        let err = client(&server)
            .issues_in_set(&named("g"))
            .await
            .unwrap_err();
        assert!(matches!(err, LinearApiError::TooLarge));
        assert_eq!(server.received_requests().await.unwrap().len(), MAX_PAGES);
    }

    #[tokio::test]
    async fn issue_key_makes_a_single_query() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!({ "data": { "issue": {
                    "identifier": "NEX-9", "state": { "type": "started" }, "trashed": false
                }}})),
            )
            .expect(1)
            .mount(&server)
            .await;
        let got = client(&server)
            .issues_in_set(&GroupKey::Issue("NEX-9".into()))
            .await
            .unwrap();
        assert_eq!(got.len(), 1);
        let body: Value =
            serde_json::from_slice(&server.received_requests().await.unwrap()[0].body).unwrap();
        assert_eq!(body["variables"]["id"], "NEX-9");
    }

    #[tokio::test]
    async fn issue_trashed_or_missing_is_empty() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!({ "data": { "issue": {
                    "identifier": "NEX-9", "state": { "type": "started" }, "trashed": true
                }}})),
            )
            .mount(&server)
            .await;
        let key = GroupKey::Issue("NEX-9".into());
        assert!(client(&server)
            .issues_in_set(&key)
            .await
            .unwrap()
            .is_empty());

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": null, "errors": [{ "message": "Entity not found: Issue" }]
            })))
            .mount(&server)
            .await;
        assert!(client(&server)
            .issues_in_set(&key)
            .await
            .unwrap()
            .is_empty());
    }

    #[tokio::test]
    async fn unresolved_is_not_a_set() {
        let server = MockServer::start().await;
        let err = client(&server)
            .issues_in_set(&GroupKey::Unresolved)
            .await
            .unwrap_err();
        assert!(matches!(err, LinearApiError::NotASet));
    }

    #[test]
    fn debug_redacts_the_key() {
        let c = LinearGraphql::new(KEY, None).unwrap();
        assert!(!format!("{c:?}").contains(KEY));
    }
}
