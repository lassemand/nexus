//! Linear webhook payloads and the transition-only dispatch filter.

use serde::{Deserialize, Serialize};

/// Top-level Linear webhook envelope.
///
/// `updated_from` is only sent on `update` actions and holds the *previous*
/// values of the fields that changed, which is how a genuine state transition is
/// told apart from an edit to an issue that was already in Todo.
#[derive(Debug, Deserialize)]
pub struct LinearWebhook {
    /// `create`, `update` or `remove`.
    pub action: Option<String>,
    /// Entity type, e.g. `Issue`.
    #[serde(rename = "type")]
    pub event_type: Option<String>,
    pub data: Option<LinearData>,
    /// Previous values of changed fields; `stateId` present only if state changed.
    #[serde(rename = "updatedFrom")]
    pub updated_from: Option<serde_json::Value>,
}

/// The `data` object of an Issue webhook.
#[derive(Debug, Deserialize)]
pub struct LinearData {
    pub id: Option<String>,
    pub identifier: Option<String>,
    pub title: Option<String>,
    pub description: Option<String>,
    pub state: Option<LinearState>,
    pub labels: Option<Vec<LinearLabel>>,
}

/// Workflow state of an issue.
#[derive(Debug, Deserialize)]
pub struct LinearState {
    pub name: Option<String>,
}

/// A label attached to an issue.
#[derive(Debug, Deserialize)]
pub struct LinearLabel {
    pub name: String,
}

/// Prefix marking a label as selecting a dispatch session group.
const SESSION_LABEL_PREFIX: &str = "session:";

/// A Linear issue reduced to what a consumer needs to act on it.
#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct LinearIssue {
    pub identifier: String,
    pub title: String,
    pub description: String,
    /// Every label name, unchanged.
    pub labels: Vec<String>,
    /// Names of `session:<group>` labels with the prefix stripped, in payload order.
    pub session_labels: Vec<String>,
}

/// Extracts session group names from label names, preserving payload order.
///
/// Only an exact `session:` prefix counts: a label merely *containing* the word
/// (`my-session`, `sessions:foo`) is not a session label.
fn session_labels(labels: &[String]) -> Vec<String> {
    labels
        .iter()
        .filter_map(|name| name.strip_prefix(SESSION_LABEL_PREFIX))
        .map(str::to_string)
        .collect()
}

/// Returns the issue if this webhook represents a *transition into* Todo.
///
/// Dispatchable when the issue is in Todo **and** either it was just created, or
/// this update changed the state (`updatedFrom.stateId` present). Editing the
/// title or description of an issue already sitting in Todo yields `None`, which
/// is what stops the same issue being dispatched repeatedly.
pub fn dispatchable_issue(payload: LinearWebhook) -> Option<LinearIssue> {
    let LinearWebhook {
        action,
        event_type,
        data,
        updated_from,
    } = payload;

    if event_type.as_deref() != Some("Issue") {
        return None;
    }

    let LinearData {
        id,
        identifier,
        title,
        description,
        state,
        labels,
    } = data?;

    if state.and_then(|s| s.name).as_deref() != Some("Todo") {
        return None;
    }

    let transitioned = match action.as_deref() {
        Some("create") => true,
        Some("update") => updated_from
            .as_ref()
            .and_then(|v| v.get("stateId"))
            .is_some(),
        _ => false,
    };
    if !transitioned {
        return None;
    }

    let labels: Vec<String> = labels
        .unwrap_or_default()
        .into_iter()
        .map(|l| l.name)
        .collect();

    Some(LinearIssue {
        identifier: identifier.or(id).unwrap_or_default(),
        title: title.unwrap_or_default(),
        description: description.unwrap_or_default(),
        session_labels: session_labels(&labels),
        labels,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn webhook(
        action: &str,
        state: &str,
        updated_from: Option<serde_json::Value>,
    ) -> LinearWebhook {
        let mut v = json!({
            "action": action,
            "type": "Issue",
            "data": {
                "id": "uuid-1",
                "identifier": "NEX-1",
                "title": "t",
                "description": "d",
                "state": { "name": state },
                "labels": []
            }
        });
        if let Some(uf) = updated_from {
            v["updatedFrom"] = uf;
        }
        serde_json::from_value(v).expect("valid payload")
    }

    #[test]
    fn create_in_todo_dispatches() {
        assert!(dispatchable_issue(webhook("create", "Todo", None)).is_some());
    }

    #[test]
    fn update_with_state_change_dispatches() {
        let uf = json!({ "stateId": "previous-state-uuid" });
        assert!(dispatchable_issue(webhook("update", "Todo", Some(uf))).is_some());
    }

    #[test]
    fn update_without_state_change_does_not_dispatch() {
        // Title edited on an issue already in Todo — no stateId in updatedFrom.
        let uf = json!({ "title": "old title" });
        assert!(dispatchable_issue(webhook("update", "Todo", Some(uf))).is_none());
    }

    #[test]
    fn update_with_no_updated_from_does_not_dispatch() {
        assert!(dispatchable_issue(webhook("update", "Todo", None)).is_none());
    }

    #[test]
    fn non_todo_state_does_not_dispatch() {
        let uf = json!({ "stateId": "previous-state-uuid" });
        assert!(dispatchable_issue(webhook("update", "In Progress", Some(uf))).is_none());
        assert!(dispatchable_issue(webhook("create", "Backlog", None)).is_none());
    }

    #[test]
    fn non_issue_type_does_not_dispatch() {
        let payload: LinearWebhook = serde_json::from_value(json!({
            "action": "create",
            "type": "Comment",
            "data": { "state": { "name": "Todo" } }
        }))
        .expect("valid payload");
        assert!(dispatchable_issue(payload).is_none());
    }

    #[test]
    fn remove_action_does_not_dispatch() {
        assert!(dispatchable_issue(webhook("remove", "Todo", None)).is_none());
    }

    fn issue_with_labels(names: &[&str]) -> LinearIssue {
        let payload: LinearWebhook = serde_json::from_value(json!({
            "action": "create",
            "type": "Issue",
            "data": {
                "identifier": "NEX-1",
                "state": { "name": "Todo" },
                "labels": names.iter().map(|n| json!({ "name": n })).collect::<Vec<_>>()
            }
        }))
        .expect("valid payload");
        dispatchable_issue(payload).expect("dispatchable")
    }

    #[test]
    fn no_session_labels() {
        let issue = issue_with_labels(&["Feature", "Bug"]);
        assert!(issue.session_labels.is_empty());
        assert_eq!(issue.labels, vec!["Feature", "Bug"]);
    }

    #[test]
    fn single_session_label_is_stripped() {
        let issue = issue_with_labels(&["Feature", "session:mcp-core"]);
        assert_eq!(issue.session_labels, vec!["mcp-core"]);
        // `labels` keeps the original, unstripped names.
        assert_eq!(issue.labels, vec!["Feature", "session:mcp-core"]);
    }

    #[test]
    fn several_session_labels_keep_payload_order() {
        let issue = issue_with_labels(&["session:b", "Feature", "session:a"]);
        assert_eq!(issue.session_labels, vec!["b", "a"]);
    }

    #[test]
    fn session_not_used_as_prefix_is_ignored() {
        let issue = issue_with_labels(&["my-session", "sessions:foo", "session", "Session:Caps"]);
        assert!(
            issue.session_labels.is_empty(),
            "only an exact `session:` prefix counts, got {:?}",
            issue.session_labels
        );
    }
}
