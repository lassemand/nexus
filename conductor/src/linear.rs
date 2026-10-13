//! Linear webhook payloads and the transition-only dispatch filter.

use serde::{Deserialize, Serialize};

/// Top-level Linear webhook envelope.
///
/// `updated_from` is only sent on `update` actions and holds the *previous*
/// values of the fields that changed. It is what distinguishes a real state
/// transition from an edit to an issue that was already in Todo.
#[derive(Debug, Clone, Deserialize)]
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
#[derive(Debug, Clone, Deserialize)]
pub struct LinearData {
    pub id: Option<String>,
    pub identifier: Option<String>,
    pub title: Option<String>,
    pub description: Option<String>,
    pub state: Option<LinearState>,
    pub labels: Option<Vec<LinearLabel>>,
}

/// Workflow state of an issue.
#[derive(Debug, Clone, Deserialize)]
pub struct LinearState {
    pub name: Option<String>,
    /// Workflow state type: `triage`, `backlog`, `unstarted`, `started`,
    /// `completed` or `canceled`. Unlike the name, it cannot be renamed per team.
    #[serde(rename = "type")]
    pub state_type: Option<String>,
}

/// A label attached to an issue.
#[derive(Debug, Clone, Deserialize)]
pub struct LinearLabel {
    pub name: String,
}

/// Workflow state that triggers dispatch.
const DISPATCH_STATE: &str = "Todo";

/// Prefix marking a label as selecting a dispatch session group.
const SESSION_LABEL_PREFIX: &str = "session:";

/// A Linear issue reduced to what a consumer needs in order to act on it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct LinearIssue {
    pub identifier: String,
    pub title: String,
    pub description: String,
    /// Every label name, unchanged.
    pub labels: Vec<String>,
    /// `session:<group>` label names with the prefix stripped, in payload order.
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

    if state.and_then(|s| s.name).as_deref() != Some(DISPATCH_STATE) {
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

/// Why an issue is considered finished.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum ClosedKind {
    /// Moved to a state of type `completed`.
    Completed,
    /// Moved to a state of type `canceled`.
    Canceled,
    /// Deleted or trashed.
    Removed,
}

/// An issue that left the active workflow, reduced to what a set check needs.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ClosedIssue {
    pub identifier: String,
    /// `session:<group>` label names with the prefix stripped, in payload order.
    pub session_labels: Vec<String>,
    pub kind: ClosedKind,
}

/// Returns the issue if this webhook represents a task becoming complete.
///
/// An `update` counts only when it is a real state transition
/// (`updatedFrom.stateId` present) into a `completed` or `canceled` state type;
/// a `remove` always counts, since deleting a task can also finish its set.
pub fn closed_issue(payload: LinearWebhook) -> Option<ClosedIssue> {
    let LinearWebhook {
        action,
        event_type,
        data,
        updated_from,
    } = payload;

    if event_type.as_deref() != Some("Issue") {
        return None;
    }
    let data = data?;

    let kind = match action.as_deref() {
        Some("update") => {
            let transitioned = updated_from
                .as_ref()
                .and_then(|v| v.get("stateId"))
                .is_some();
            if !transitioned {
                return None;
            }
            match data.state.and_then(|s| s.state_type).as_deref() {
                Some("completed") => ClosedKind::Completed,
                Some("canceled") => ClosedKind::Canceled,
                _ => return None,
            }
        }
        Some("remove") => ClosedKind::Removed,
        _ => return None,
    };

    let labels: Vec<String> = data
        .labels
        .unwrap_or_default()
        .into_iter()
        .map(|l| l.name)
        .collect();

    Some(ClosedIssue {
        identifier: data.identifier.or(data.id).unwrap_or_default(),
        session_labels: session_labels(&labels),
        kind,
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
        let issue = issue_with_labels(&["Feature", "session:conductor-core"]);
        assert_eq!(issue.session_labels, vec!["conductor-core"]);
        // `labels` keeps the original, unstripped names.
        assert_eq!(issue.labels, vec!["Feature", "session:conductor-core"]);
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

    fn closed_payload(
        action: &str,
        ty: &str,
        state_type: &str,
        state_changed: bool,
    ) -> LinearWebhook {
        let mut v = json!({
            "action": action,
            "type": ty,
            "data": {
                "identifier": "NEX-1",
                "state": { "name": "Whatever", "type": state_type },
                "labels": [{ "name": "session:a" }, { "name": "Feature" }]
            }
        });
        if state_changed {
            v["updatedFrom"] = json!({ "stateId": "prev" });
        }
        serde_json::from_value(v).expect("valid payload")
    }

    #[test]
    fn update_to_completed_is_completed() {
        let c = closed_issue(closed_payload("update", "Issue", "completed", true)).unwrap();
        assert_eq!(c.kind, ClosedKind::Completed);
        assert_eq!(c.identifier, "NEX-1");
        assert_eq!(c.session_labels, vec!["a"]);
    }

    #[test]
    fn update_to_canceled_is_canceled() {
        let c = closed_issue(closed_payload("update", "Issue", "canceled", true)).unwrap();
        assert_eq!(c.kind, ClosedKind::Canceled);
    }

    #[test]
    fn completed_without_state_change_is_ignored() {
        assert!(closed_issue(closed_payload("update", "Issue", "completed", false)).is_none());
    }

    #[test]
    fn update_to_open_states_is_ignored() {
        for t in ["started", "unstarted", "backlog", "triage"] {
            assert!(closed_issue(closed_payload("update", "Issue", t, true)).is_none());
        }
    }

    #[test]
    fn remove_is_removed() {
        let c = closed_issue(closed_payload("remove", "Issue", "started", false)).unwrap();
        assert_eq!(c.kind, ClosedKind::Removed);
    }

    #[test]
    fn non_issue_is_ignored_for_closed() {
        assert!(closed_issue(closed_payload("update", "Comment", "completed", true)).is_none());
        assert!(closed_issue(closed_payload("remove", "Comment", "completed", false)).is_none());
    }

    #[test]
    fn todo_transition_yields_only_the_todo_event() {
        let mk = || {
            serde_json::from_value::<LinearWebhook>(json!({
                "action": "update",
                "type": "Issue",
                "data": { "identifier": "NEX-1", "state": { "name": "Todo", "type": "unstarted" } },
                "updatedFrom": { "stateId": "prev" }
            }))
            .unwrap()
        };
        assert!(dispatchable_issue(mk()).is_some());
        assert!(closed_issue(mk()).is_none());
    }
}
