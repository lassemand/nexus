//! Linear GraphQL clients.
//!
//! [`LinearGraphql`] reads with `LINEAR_API_KEY`: every issue in a session set,
//! a set label's description, and an issue with its pull request attachments.
//! [`LinearWriter`] makes the review orchestration's writes (comments and the
//! move back to Todo) under the reviewer's own identity, see
//! [`crate::linear_oauth`].
//!
//! Every failure is an `Err`, never an empty `Ok`: callers use the answer to
//! decide whether a session may be closed, so an uncertain answer must never
//! look like "the set is empty".

use std::time::Duration;

use std::collections::HashMap;
use std::sync::Arc;

use serde::Deserialize;
use serde_json::{json, Value};
use tokio::sync::Mutex;

use crate::linear_oauth::AppTokenSource;
use crate::registry::GroupKey;
use crate::review::{ReviewIssue, ReviewLinear};

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

/// Labels are workspace- or team-scoped, so several can share a name.
const LABEL_QUERY: &str = "query SetLabel($label: String!) { \
issueLabels(filter: { name: { eqIgnoreCase: $label } }, first: 50) { \
nodes { id description team { id } } } }";

const ISSUE_QUERY: &str =
    "query OneIssue($id: String!) { issue(id: $id) { identifier state { type } trashed } }";

/// `id` accepts an identifier such as `NEX-1` as well as the UUID.
const REVIEW_ISSUE_QUERY: &str = "query ReviewIssue($id: String!) { issue(id: $id) { \
id identifier url state { name } team { id } attachments(first: 50) { nodes { url } } } }";

const TODO_STATE_QUERY: &str = "query TodoState($team: ID!, $name: String!) { \
workflowStates(filter: { team: { id: { eq: $team } }, name: { eqIgnoreCase: $name } }, first: 5) { \
nodes { id name } } }";

const COMMENT_MUTATION: &str = "mutation ReviewComment($issueId: String!, $body: String!) { \
commentCreate(input: { issueId: $issueId, body: $body }) { success } }";

const STATE_MUTATION: &str = "mutation ReviewMove($id: String!, $stateId: String!) { \
issueUpdate(id: $id, input: { stateId: $stateId }) { success } }";

/// Name of the state a send-back moves an issue to.
pub const TODO_STATE: &str = "Todo";

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
    #[error("Linear rejected the credentials")]
    Unauthorized,
    #[error("Linear reported the write as unsuccessful")]
    Unsuccessful,
    #[error("no workflow state named {0:?} in the issue's team")]
    NoSuchState(String),
}

/// Source of the full issue list of a session group.
#[async_trait::async_trait]
pub trait IssueSetSource: Send + Sync {
    /// Lists every non-trashed issue in the group, or errors if unsure.
    async fn issues_in_set(&self, key: &GroupKey) -> Result<Vec<SetIssue>, LinearApiError>;

    /// Descriptions of every `session:<group>` label, an empty string for a
    /// label without one. An empty list means no such label exists.
    ///
    /// Defaults to `NotASet`, so a source that cannot look labels up makes the
    /// caller fall back to the global model.
    async fn set_label_descriptions(&self, _group: &str) -> Result<Vec<String>, LinearApiError> {
        Err(LinearApiError::NotASet)
    }
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
    /// `Option` because Linear sends `null` here for an issue that is not
    /// trashed, rather than `false`. `#[serde(default)]` alone does not cover
    /// that: it supplies a value for an *absent* field, while an explicit null
    /// is a type error against `bool`. Deserialising it as `bool` failed the
    /// whole response —
    ///   malformed Linear response: invalid type: null, expected a boolean
    /// — which made the auto-close sweep unable to read any set, and because
    /// the sweep fails safe the only symptom was a warning in the log while
    /// nothing ever closed.
    ///
    /// Absent, null and `false` all mean the same thing: not trashed.
    #[serde(default)]
    trashed: Option<bool>,
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
struct LabelData {
    #[serde(rename = "issueLabels")]
    issue_labels: LabelConnection,
}

#[derive(Deserialize)]
struct LabelConnection {
    nodes: Vec<LabelNode>,
}

#[derive(Deserialize)]
struct LabelNode {
    description: Option<String>,
}

#[derive(Deserialize)]
struct IssueData {
    issue: Option<Node>,
}

impl Node {
    fn into_set_issue(self) -> Option<SetIssue> {
        (!self.trashed.unwrap_or(false)).then_some(SetIssue {
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

    async fn label_descriptions(&self, group: &str) -> Result<Vec<String>, LinearApiError> {
        let label = format!("{SESSION_LABEL_PREFIX}{group}");
        let env: Envelope<LabelData> = self.post(LABEL_QUERY, json!({ "label": label })).await?;
        if let Some(errors) = env.errors.filter(|e| !e.is_empty()) {
            return Err(LinearApiError::GraphQl(Value::Array(errors).to_string()));
        }
        let data = env
            .data
            .ok_or_else(|| LinearApiError::Malformed("missing data".into()))?;
        Ok(data
            .issue_labels
            .nodes
            .into_iter()
            .map(|n| n.description.unwrap_or_default())
            .collect())
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

#[derive(Deserialize)]
struct ReviewIssueData {
    issue: Option<ReviewIssueNode>,
}

#[derive(Deserialize)]
struct ReviewIssueNode {
    id: String,
    identifier: String,
    url: Option<String>,
    state: Option<NamedNode>,
    team: Option<IdNode>,
    attachments: Option<UrlConnection>,
}

#[derive(Deserialize)]
struct NamedNode {
    name: Option<String>,
}

#[derive(Deserialize)]
struct IdNode {
    id: String,
}

#[derive(Deserialize)]
struct UrlConnection {
    nodes: Vec<UrlNode>,
}

#[derive(Deserialize)]
struct UrlNode {
    url: Option<String>,
}

#[derive(Deserialize)]
struct WorkflowStateData {
    #[serde(rename = "workflowStates")]
    workflow_states: StateConnection,
}

#[derive(Deserialize)]
struct StateConnection {
    nodes: Vec<IdNode>,
}

impl LinearGraphql {
    /// An issue with its pull request attachments, by UUID or identifier.
    /// `Ok(None)` when Linear says it does not exist.
    pub async fn review_issue(&self, id: &str) -> Result<Option<ReviewIssue>, LinearApiError> {
        let env: Envelope<ReviewIssueData> =
            self.post(REVIEW_ISSUE_QUERY, json!({ "id": id })).await?;
        if let Some(errors) = env.errors.filter(|e| !e.is_empty()) {
            let not_found = errors.iter().all(|e| {
                e.get("message")
                    .and_then(Value::as_str)
                    .is_some_and(|m| m.contains("Entity not found"))
            });
            if not_found {
                return Ok(None);
            }
            return Err(LinearApiError::GraphQl(Value::Array(errors).to_string()));
        }
        let data = env
            .data
            .ok_or_else(|| LinearApiError::Malformed("missing data".into()))?;
        Ok(data.issue.map(|node| ReviewIssue {
            id: node.id,
            identifier: node.identifier,
            url: node.url.unwrap_or_default(),
            state_name: node.state.and_then(|s| s.name).unwrap_or_default(),
            team_id: node.team.map(|t| t.id).unwrap_or_default(),
            attachment_urls: node
                .attachments
                .map(|a| a.nodes.into_iter().filter_map(|n| n.url).collect())
                .unwrap_or_default(),
        }))
    }

    /// The id of the team's workflow state with this name.
    pub async fn state_id(&self, team_id: &str, name: &str) -> Result<String, LinearApiError> {
        let env: Envelope<WorkflowStateData> = self
            .post(TODO_STATE_QUERY, json!({ "team": team_id, "name": name }))
            .await?;
        if let Some(errors) = env.errors.filter(|e| !e.is_empty()) {
            return Err(LinearApiError::GraphQl(Value::Array(errors).to_string()));
        }
        env.data
            .ok_or_else(|| LinearApiError::Malformed("missing data".into()))?
            .workflow_states
            .nodes
            .into_iter()
            .next()
            .map(|n| n.id)
            .ok_or_else(|| LinearApiError::NoSuchState(name.to_string()))
    }
}

/// Whose name a Linear write appears under.
#[derive(Clone)]
pub enum LinearIdentity {
    /// The reviewer's OAuth app, acting as itself.
    App(Arc<AppTokenSource>),
    /// `LINEAR_API_KEY`, used when the app is not configured.
    ApiKey(String),
}

impl LinearIdentity {
    /// The identity review writes use: the OAuth app when both its client id
    /// and secret are set, otherwise `api_key` with one warning. Sets the
    /// `conductor_review_linear_identity` gauge.
    pub fn for_reviews(
        client_id: Option<String>,
        client_secret: Option<String>,
        api_key: &str,
        token_url: Option<String>,
    ) -> Result<Self, LinearApiError> {
        let client_id = client_id.filter(|v| !v.is_empty());
        let client_secret = client_secret.filter(|v| !v.is_empty());
        let identity = match (client_id, client_secret) {
            (Some(id), Some(secret)) => {
                LinearIdentity::App(Arc::new(AppTokenSource::new(id, secret, token_url)?))
            }
            (id, _) => {
                let missing = if id.is_some() {
                    "REVIEW_LINEAR_CLIENT_SECRET"
                } else {
                    "REVIEW_LINEAR_CLIENT_ID"
                };
                tracing::warn!(
                    "{missing} not set; review comments and state changes are made as LINEAR_API_KEY"
                );
                LinearIdentity::ApiKey(api_key.to_string())
            }
        };
        for kind in ["app", "fallback"] {
            metrics::gauge!("conductor_review_linear_identity", "kind" => kind)
                .set(if kind == identity.kind() { 1.0 } else { 0.0 });
        }
        Ok(identity)
    }

    /// The metric label for this identity.
    pub fn kind(&self) -> &'static str {
        match self {
            LinearIdentity::App(_) => "app",
            LinearIdentity::ApiKey(_) => "fallback",
        }
    }

    async fn authorization(&self) -> Result<String, LinearApiError> {
        match self {
            LinearIdentity::App(source) => Ok(format!("Bearer {}", source.token().await?)),
            LinearIdentity::ApiKey(key) => Ok(key.clone()),
        }
    }
}

/// Makes the review orchestration's Linear writes under one identity.
///
/// An app token Linear rejects is renewed once and the write retried; any
/// further failure is returned, and the caller retries with backoff. It never
/// switches to `LINEAR_API_KEY` mid-flight, so a review's comments always share
/// one author.
pub struct LinearWriter {
    http: reqwest::Client,
    url: String,
    identity: LinearIdentity,
}

impl std::fmt::Debug for LinearWriter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LinearWriter")
            .field("url", &self.url)
            .field("identity", &self.identity.kind())
            .finish()
    }
}

#[derive(Deserialize)]
struct MutationEnvelope {
    data: Option<Value>,
    errors: Option<Vec<Value>>,
}

impl LinearWriter {
    /// Builds a writer posting to `url` (Linear's GraphQL endpoint by default).
    pub fn new(identity: LinearIdentity, url: Option<String>) -> Result<Self, LinearApiError> {
        let http = reqwest::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .build()
            .map_err(|e| LinearApiError::Network(e.to_string()))?;
        Ok(Self {
            http,
            url: url.unwrap_or_else(|| DEFAULT_API_URL.to_string()),
            identity,
        })
    }

    /// The identity writes are made under.
    pub fn identity(&self) -> &LinearIdentity {
        &self.identity
    }

    /// Posts a comment on an issue.
    pub async fn comment(&self, issue_id: &str, body: &str) -> Result<(), LinearApiError> {
        self.mutate(
            COMMENT_MUTATION,
            json!({ "issueId": issue_id, "body": body }),
            "commentCreate",
        )
        .await
    }

    /// Moves an issue to a workflow state.
    pub async fn set_state(&self, issue_id: &str, state_id: &str) -> Result<(), LinearApiError> {
        self.mutate(
            STATE_MUTATION,
            json!({ "id": issue_id, "stateId": state_id }),
            "issueUpdate",
        )
        .await
    }

    async fn mutate(
        &self,
        query: &str,
        variables: Value,
        field: &str,
    ) -> Result<(), LinearApiError> {
        let first = self.mutate_once(query, &variables, field).await;
        let result = match (&first, &self.identity) {
            (Err(LinearApiError::Unauthorized), LinearIdentity::App(source)) => {
                metrics::counter!("conductor_review_linear_auth_failures_total").increment(1);
                tracing::warn!("Linear rejected the reviewer's app token; renewing it once");
                source.invalidate().await;
                self.mutate_once(query, &variables, field).await
            }
            _ => first,
        };
        if matches!(result, Err(LinearApiError::Unauthorized)) {
            metrics::counter!("conductor_review_linear_auth_failures_total").increment(1);
        }
        if result.is_err() {
            metrics::counter!("conductor_linear_api_errors_total").increment(1);
        }
        result
    }

    async fn mutate_once(
        &self,
        query: &str,
        variables: &Value,
        field: &str,
    ) -> Result<(), LinearApiError> {
        let authorization = self.identity.authorization().await?;
        let response = self
            .http
            .post(&self.url)
            .header("Authorization", authorization)
            .json(&json!({ "query": query, "variables": variables }))
            .send()
            .await
            .map_err(|e| LinearApiError::Network(e.without_url().to_string()))?;
        let status = response.status().as_u16();
        if status == 401 {
            return Err(LinearApiError::Unauthorized);
        }
        let bytes = response
            .bytes()
            .await
            .map_err(|e| LinearApiError::Network(e.without_url().to_string()))?;
        let envelope: Option<MutationEnvelope> = serde_json::from_slice(&bytes).ok();
        let errors = envelope
            .as_ref()
            .and_then(|e| e.errors.clone())
            .filter(|e| !e.is_empty());
        if let Some(errors) = &errors {
            let unauthenticated = errors.iter().any(|e| {
                e.pointer("/extensions/code")
                    .and_then(Value::as_str)
                    .is_some_and(|c| c.eq_ignore_ascii_case("AUTHENTICATION_ERROR"))
            });
            if unauthenticated {
                return Err(LinearApiError::Unauthorized);
            }
        }
        if status != 200 {
            return Err(LinearApiError::Status(status));
        }
        if let Some(errors) = errors {
            return Err(LinearApiError::GraphQl(Value::Array(errors).to_string()));
        }
        let success = envelope.and_then(|e| e.data).and_then(|d| {
            d.get(field)
                .and_then(|f| f.get("success"))
                .and_then(Value::as_bool)
        });
        match success {
            Some(true) => Ok(()),
            Some(false) => Err(LinearApiError::Unsuccessful),
            None => Err(LinearApiError::Malformed(format!("no {field}.success"))),
        }
    }
}

/// [`ReviewLinear`] over Linear's API: reads with `LINEAR_API_KEY`, writes
/// through a [`LinearWriter`].
pub struct LinearReviewApi {
    reader: LinearGraphql,
    writer: LinearWriter,
    /// Todo state id per team; workflow states almost never change.
    todo_states: Mutex<HashMap<String, String>>,
}

impl LinearReviewApi {
    /// Combines a reader and a writer.
    pub fn new(reader: LinearGraphql, writer: LinearWriter) -> Self {
        Self {
            reader,
            writer,
            todo_states: Mutex::new(HashMap::new()),
        }
    }

    async fn todo_state(&self, team_id: &str) -> Result<String, LinearApiError> {
        if let Some(id) = self.todo_states.lock().await.get(team_id) {
            return Ok(id.clone());
        }
        let id = self.reader.state_id(team_id, TODO_STATE).await?;
        self.todo_states
            .lock()
            .await
            .insert(team_id.to_string(), id.clone());
        Ok(id)
    }
}

#[async_trait::async_trait]
impl ReviewLinear for LinearReviewApi {
    async fn issue(&self, id: &str) -> Result<Option<ReviewIssue>, LinearApiError> {
        let result = self.reader.review_issue(id).await;
        if result.is_err() {
            metrics::counter!("conductor_linear_api_errors_total").increment(1);
        }
        result
    }

    async fn comment(&self, issue_id: &str, body: &str) -> Result<(), LinearApiError> {
        self.writer.comment(issue_id, body).await
    }

    async fn move_to_todo(&self, issue: &ReviewIssue) -> Result<(), LinearApiError> {
        let state = self.todo_state(&issue.team_id).await?;
        self.writer.set_state(&issue.id, &state).await
    }
}

#[async_trait::async_trait]
impl IssueSetSource for LinearGraphql {
    async fn issues_in_set(&self, key: &GroupKey) -> Result<Vec<SetIssue>, LinearApiError> {
        let result = match key {
            GroupKey::Named(group) => self.named_set(group).await,
            GroupKey::Issue(id) => self.single(id).await,
            GroupKey::Unresolved | GroupKey::Review(_) => Err(LinearApiError::NotASet),
        };
        // `NotASet` is a caller mistake, not a failing API.
        if matches!(&result, Err(e) if !matches!(e, LinearApiError::NotASet)) {
            metrics::counter!("conductor_linear_api_errors_total").increment(1);
        }
        result
    }

    async fn set_label_descriptions(&self, group: &str) -> Result<Vec<String>, LinearApiError> {
        let result = self.label_descriptions(group).await;
        if result.is_err() {
            metrics::counter!("conductor_linear_api_errors_total").increment(1);
        }
        result
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
    async fn trashed_null_means_not_trashed() {
        // What Linear actually sends for an issue that is not trashed. Parsing
        // this as `bool` failed the entire response, so a single such issue
        // blocked auto-close for the whole set.
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!({ "data": { "issues": {
                    "nodes": [
                        { "identifier": "NEX-1", "state": { "type": "completed" }, "trashed": null },
                        { "identifier": "NEX-2", "state": { "type": "started" } },
                    ],
                    "pageInfo": { "hasNextPage": false, "endCursor": null }
                }}})),
            )
            .mount(&server)
            .await;
        let got = client(&server).issues_in_set(&named("g")).await.unwrap();
        assert_eq!(
            got.iter()
                .map(|i| i.identifier.as_str())
                .collect::<Vec<_>>(),
            ["NEX-1", "NEX-2"],
            "null and absent both mean not trashed"
        );
    }

    #[tokio::test]
    async fn trashed_null_on_the_single_issue_query_too() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!({ "data": { "issue": {
                    "identifier": "NEX-9", "state": { "type": "started" }, "trashed": null
                }}})),
            )
            .mount(&server)
            .await;
        let got = client(&server)
            .issues_in_set(&GroupKey::Issue("NEX-9".into()))
            .await
            .unwrap();
        assert_eq!(got.len(), 1, "a null trashed must not drop the issue");
        assert_eq!(got[0].identifier, "NEX-9");
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

    fn labels(descriptions: &[Value]) -> ResponseTemplate {
        let nodes: Vec<_> = descriptions
            .iter()
            .map(|d| json!({ "id": "l", "description": d, "team": null }))
            .collect();
        ResponseTemplate::new(200)
            .set_body_json(json!({ "data": { "issueLabels": { "nodes": nodes } } }))
    }

    #[tokio::test]
    async fn label_found_with_description() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(header("Authorization", KEY))
            .respond_with(labels(&[json!("Metrics.\nmodel: opus")]))
            .expect(1)
            .mount(&server)
            .await;
        let got = client(&server).set_label_descriptions("grp").await.unwrap();
        assert_eq!(got, ["Metrics.\nmodel: opus"]);
        let body: Value =
            serde_json::from_slice(&server.received_requests().await.unwrap()[0].body).unwrap();
        assert!(body["query"]
            .as_str()
            .unwrap()
            .contains("eqIgnoreCase: $label"));
        assert_eq!(body["variables"]["label"], "session:grp");
    }

    #[tokio::test]
    async fn label_not_found_is_empty_and_null_description_is_blank() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(labels(&[]))
            .mount(&server)
            .await;
        assert!(client(&server)
            .set_label_descriptions("g")
            .await
            .unwrap()
            .is_empty());

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(labels(&[Value::Null]))
            .mount(&server)
            .await;
        assert_eq!(
            client(&server).set_label_descriptions("g").await.unwrap(),
            [""]
        );
    }

    #[tokio::test]
    async fn label_lookup_http_500_and_graphql_errors_fail() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;
        let err = client(&server)
            .set_label_descriptions("g")
            .await
            .unwrap_err();
        assert!(matches!(err, LinearApiError::Status(500)));

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({ "data": null, "errors": [{ "message": "boom" }] })),
            )
            .mount(&server)
            .await;
        let err = client(&server)
            .set_label_descriptions("g")
            .await
            .unwrap_err();
        assert!(matches!(err, LinearApiError::GraphQl(_)));
    }

    #[tokio::test]
    async fn two_labels_with_the_name_are_both_returned() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(labels(&[json!("model: opus"), json!("model: sonnet")]))
            .mount(&server)
            .await;
        let got = client(&server).set_label_descriptions("g").await.unwrap();
        assert_eq!(got, ["model: opus", "model: sonnet"]);
    }

    #[test]
    fn debug_redacts_the_key() {
        let c = LinearGraphql::new(KEY, None).unwrap();
        assert!(!format!("{c:?}").contains(KEY));
    }

    fn review_issue() -> ReviewIssue {
        ReviewIssue {
            id: "uuid-1".into(),
            identifier: "NEX-1".into(),
            url: "u".into(),
            state_name: "In Review".into(),
            team_id: "team-1".into(),
            attachment_urls: vec![],
        }
    }

    fn graphql_responder(req: &Request) -> ResponseTemplate {
        let body: Value = serde_json::from_slice(&req.body).unwrap();
        let q = body["query"].as_str().unwrap();
        if q.contains("workflowStates") {
            ResponseTemplate::new(200).set_body_json(
                json!({ "data": { "workflowStates": { "nodes": [{ "id": "todo-1", "name": "Todo" }] } } }),
            )
        } else if q.contains("commentCreate") {
            ResponseTemplate::new(200)
                .set_body_json(json!({ "data": { "commentCreate": { "success": true } } }))
        } else {
            ResponseTemplate::new(200)
                .set_body_json(json!({ "data": { "issueUpdate": { "success": true } } }))
        }
    }

    fn auth_of(req: &Request) -> String {
        req.headers
            .get("authorization")
            .unwrap()
            .to_str()
            .unwrap()
            .to_string()
    }

    fn query_of(req: &Request) -> String {
        let body: Value = serde_json::from_slice(&req.body).unwrap();
        body["query"].as_str().unwrap().to_string()
    }

    #[tokio::test]
    async fn with_app_credentials_the_send_back_writes_use_the_app_token() {
        let oauth = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(
                json!({ "access_token": "app-tok", "token_type": "Bearer", "expires_in": 2591999 }),
            ))
            .mount(&oauth)
            .await;
        let api = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(graphql_responder)
            .mount(&api)
            .await;

        let identity = LinearIdentity::for_reviews(
            Some("cid".into()),
            Some("secret".into()),
            KEY,
            Some(oauth.uri()),
        )
        .unwrap();
        assert_eq!(identity.kind(), "app");
        let review = LinearReviewApi::new(
            client(&api),
            LinearWriter::new(identity, Some(api.uri())).unwrap(),
        );
        review
            .comment("uuid-1", "Review send-back 1 of 2")
            .await
            .unwrap();
        review.move_to_todo(&review_issue()).await.unwrap();

        let requests = api.received_requests().await.unwrap();
        let writes: Vec<_> = requests
            .iter()
            .filter(|r| query_of(r).starts_with("mutation"))
            .collect();
        assert_eq!(writes.len(), 2, "a comment and a state change");
        for w in &writes {
            assert_eq!(auth_of(w), "Bearer app-tok");
        }
        let moved: Value = serde_json::from_slice(&writes[1].body).unwrap();
        assert_eq!(moved["variables"]["stateId"], "todo-1");
        assert_eq!(moved["variables"]["id"], "uuid-1");
    }

    #[tokio::test]
    async fn without_app_credentials_writes_fall_back_to_the_api_key() {
        let api = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(graphql_responder)
            .mount(&api)
            .await;
        let identity = LinearIdentity::for_reviews(None, None, KEY, None).unwrap();
        assert_eq!(identity.kind(), "fallback");
        let writer = LinearWriter::new(identity, Some(api.uri())).unwrap();
        writer.comment("uuid-1", "hello").await.unwrap();
        let requests = api.received_requests().await.unwrap();
        assert_eq!(auth_of(&requests[0]), KEY);
    }

    #[tokio::test]
    async fn an_expired_app_token_is_renewed_once_and_the_write_retried() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let oauth = MockServer::start().await;
        let issued = std::sync::Arc::new(AtomicUsize::new(0));
        let counter = std::sync::Arc::clone(&issued);
        Mock::given(method("POST"))
            .respond_with(move |_: &Request| {
                let n = counter.fetch_add(1, Ordering::SeqCst) + 1;
                ResponseTemplate::new(200).set_body_json(
                    json!({ "access_token": format!("t{n}"), "expires_in": 2591999 }),
                )
            })
            .mount(&oauth)
            .await;
        let api = MockServer::start().await;
        Mock::given(method("POST"))
            .and(header("Authorization", "Bearer t1"))
            .respond_with(ResponseTemplate::new(401))
            .mount(&api)
            .await;
        Mock::given(method("POST"))
            .and(header("Authorization", "Bearer t2"))
            .respond_with(graphql_responder)
            .mount(&api)
            .await;
        let identity = LinearIdentity::for_reviews(
            Some("cid".into()),
            Some("s".into()),
            KEY,
            Some(oauth.uri()),
        )
        .unwrap();
        let writer = LinearWriter::new(identity, Some(api.uri())).unwrap();
        writer.comment("uuid-1", "hello").await.unwrap();
        assert_eq!(issued.load(Ordering::SeqCst), 2, "renewed exactly once");
        assert_eq!(api.received_requests().await.unwrap().len(), 2);
    }

    #[tokio::test]
    async fn an_authentication_error_in_the_body_counts_as_unauthorized() {
        let api = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(400).set_body_json(json!({
                "errors": [{ "message": "Authentication required", "extensions": { "code": "AUTHENTICATION_ERROR" } }]
            })))
            .mount(&api)
            .await;
        let writer =
            LinearWriter::new(LinearIdentity::ApiKey(KEY.into()), Some(api.uri())).unwrap();
        assert!(matches!(
            writer.comment("uuid-1", "x").await,
            Err(LinearApiError::Unauthorized)
        ));
    }

    #[tokio::test]
    async fn a_review_issue_carries_its_attachments() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "data": { "issue": {
                "id": "uuid-1", "identifier": "NEX-1", "url": "https://linear.app/x/issue/NEX-1",
                "state": { "name": "In Review" }, "team": { "id": "team-1" },
                "attachments": { "nodes": [{ "url": "https://github.com/o/r/pull/3" }, { "url": null }] }
            }}})))
            .mount(&server)
            .await;
        let issue = client(&server)
            .review_issue("NEX-1")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(issue.id, "uuid-1");
        assert_eq!(issue.team_id, "team-1");
        assert_eq!(
            issue.attachment_urls,
            vec!["https://github.com/o/r/pull/3".to_string()]
        );
    }
}
