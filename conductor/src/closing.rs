//! Closing a session group: remove its worktree and forget it.
//!
//! Shared by the operator's `sessions close` and the dispatcher's automatic
//! close, so both apply the same checks in the same order.
//!
//! Everything happens inside one registry transaction. That transaction holds
//! the registry-wide advisory lock and the group's row lock, so an event for the
//! same set either lands before the check (and the close backs off because work
//! is pending) or after the commit (and builds a fresh group). Ingest creates
//! the group and queues the event under that same advisory lock, so the row can
//! never vanish between the two steps.

use std::path::Path;

use crate::dispatcher::run_git;
use crate::registry::{GroupKey, GroupState, Registry, RunResult};
use crate::tmux::SessionRuntime;

/// Who is closing, and how much they may override.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CloseMode {
    /// Dispatcher-initiated. Never forces, never discards queued events.
    Auto,
    /// Operator-initiated. With `force`, queued events are discarded and the
    /// worktree is removed even if dirty. The caller has already stopped any
    /// running process.
    Manual { force: bool },
}

impl CloseMode {
    fn force(self) -> bool {
        matches!(self, CloseMode::Manual { force: true })
    }

    fn label(self) -> &'static str {
        match self {
            CloseMode::Auto => "auto",
            CloseMode::Manual { .. } => "manual",
        }
    }
}

/// What a close attempt did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CloseOutcome {
    /// The worktree is removed and the group forgotten.
    Closed {
        branches: Vec<String>,
        issues: Vec<String>,
    },
    /// A process is running or attached.
    Busy,
    /// Events are queued.
    HasPending,
    /// `git worktree remove` refused; the group is untouched. Carries git's error.
    WorktreeDirty(String),
    /// The group no longer exists.
    Gone,
}

/// Closes a group if it is safe to, reporting why not otherwise.
///
/// Only the worktree and the registry row are removed; branches and Claude
/// transcripts stay. If the worktree cannot be removed, nothing is deleted.
///
/// A set is closable while `Ready` or without a session, provided nobody is
/// attached to its tmux session; the session is killed before the worktree goes.
/// With `runtime: None` a live tmux session counts as busy.
pub async fn close_group(
    registry: &Registry,
    repo_root: &Path,
    key: &GroupKey,
    mode: CloseMode,
    runtime: Option<&dyn SessionRuntime>,
) -> Result<CloseOutcome, sqlx::Error> {
    let slug = key.slug();

    // The tmux session is ended first, in its own short transaction, so no row
    // lock is held across a tmux call. The checks are repeated under the lock.
    if let (Some(rt), Some(group)) = (runtime, registry.group_by_slug(&slug).await?) {
        let live = group.state.is_tmux() && group.state != GroupState::NoSession;
        if live {
            if !mode.force() {
                if !matches!(group.state, GroupState::Ready { .. }) {
                    return Ok(CloseOutcome::Busy);
                }
                if !group.pending.is_empty() {
                    return Ok(CloseOutcome::HasPending);
                }
                if !rt.clients(&slug).await.is_empty() {
                    return Ok(CloseOutcome::Busy);
                }
            }
            let mut txn = registry.begin().await?;
            let ended = txn
                .end_session(
                    key,
                    RunResult::Interrupted,
                    Some(&group.state),
                    chrono::Duration::zero(),
                )
                .await?;
            txn.commit().await?;
            if !ended {
                // It changed under us; let the caller try again later.
                return Ok(CloseOutcome::Busy);
            }
            if let Err(e) = rt.kill(&slug).await {
                tracing::error!(group = %slug, error = %e, "cannot kill the session being closed");
            }
        }
    }

    let mut txn = registry.begin().await?;

    let Some(group) = txn.group_by_slug(&key.slug()).await? else {
        return Ok(CloseOutcome::Gone);
    };

    if !mode.force() {
        if !matches!(group.state, GroupState::Idle | GroupState::NoSession) {
            return Ok(CloseOutcome::Busy);
        }
        if !group.pending.is_empty() {
            return Ok(CloseOutcome::HasPending);
        }
    }

    // Git before the delete: a refusal must leave the group intact, or the
    // registry would forget a worktree that is still on disk.
    if !group.worktree.as_os_str().is_empty() && group.worktree.exists() {
        let path = group.worktree.to_string_lossy().to_string();
        let args: Vec<&str> = if mode.force() {
            vec!["worktree", "remove", "--force", &path]
        } else {
            vec!["worktree", "remove", &path]
        };
        if let Err(e) = run_git(repo_root, &args).await {
            return Ok(CloseOutcome::WorktreeDirty(e.to_string()));
        }
    }

    if !txn.delete_group(&group.key).await? {
        return Ok(CloseOutcome::Gone);
    }
    txn.record_closed(&group, mode.label()).await?;
    txn.commit().await?;

    let branches: Vec<String> = group.branches.iter().cloned().collect();
    if mode == CloseMode::Auto {
        tracing::info!(
            group = %group.key.slug(),
            issues = ?group.issues,
            branches = ?branches,
            session_id = %group.session_id,
            "set complete; group closed automatically"
        );
    }
    Ok(CloseOutcome::Closed {
        branches,
        issues: group.issues,
    })
}
