//! Persistent session registry: which Claude session serves which group.
//!
//! Backed by Postgres. Two processes touch it — the long-running
//! `conductor serve` and `conductor sessions ...` invoked through
//! `kubectl exec` — so every mutation runs inside a transaction holding a
//! registry-wide advisory lock.
//!
//! The lock is held only across a read-modify-write, never while a `claude`
//! process runs; a long run is represented by [`GroupState::Running`] instead.

use std::collections::{BTreeSet, VecDeque};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::{postgres::PgRow, types::Json, PgPool, Postgres, Row, Transaction};
use uuid::Uuid;

use crate::InboundEvent;

/// Environment variable holding the Postgres connection string.
pub const DATABASE_URL_ENV: &str = "DATABASE_URL";
/// Environment variable overriding the idle expiry window, in days.
pub const IDLE_DAYS_ENV: &str = "CONDUCTOR_IDLE_DAYS";
/// Days a group may sit idle before its session is considered expired.
pub const DEFAULT_IDLE_DAYS: i64 = 7;
/// Upper bound on remembered delivery IDs, evicted oldest-first.
pub const MAX_SEEN_DELIVERIES: usize = 1000;
/// Longest a named group key may be.
const MAX_KEY_LEN: usize = 63;
/// Tolerance when matching a recorded process start time against the live one.
#[cfg(target_os = "linux")]
const START_TIME_TOLERANCE: Duration = Duration::from_secs(2);

/// Key for the registry-wide advisory lock ("cond" in hex).
///
/// One lock for the whole registry rather than one per group: the deployment
/// runs a single replica, and coarse locking removes any chance of two
/// operations interleaving on related rows.
const REGISTRY_LOCK_KEY: i64 = 0x636F_6E64;

// ── time ─────────────────────────────────────────────────────────────────────

/// Source of the current time, injectable so tests need not sleep.
pub trait Clock: Send + Sync + 'static {
    fn now(&self) -> DateTime<Utc>;
}

/// Wall-clock time.
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> DateTime<Utc> {
        Utc::now()
    }
}

// ── keys ─────────────────────────────────────────────────────────────────────

/// Identifies the session group an event belongs to.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
pub enum GroupKey {
    /// From a single `session:<name>` label; shared by every issue so labelled.
    Named(String),
    /// An unlabelled issue, which gets a session of its own.
    Issue(String),
    /// A GitHub PR comment, whose group needs a branch lookup to resolve
    /// (NEX-130). Never used as a registry key.
    Unresolved,
}

impl GroupKey {
    /// Filesystem- and git-safe name for this group.
    ///
    /// Note two distinct keys can collide here: a named key `linear-nex-42` and
    /// the issue key `NEX-42` both slug to `linear-nex-42`, and would therefore
    /// share a group. Accepted rather than worked around, since the slug format
    /// is prescribed; naming a group `linear-*` is the thing to avoid.
    pub fn slug(&self) -> String {
        match self {
            GroupKey::Named(name) => name.clone(),
            GroupKey::Issue(identifier) => format!("linear-{}", identifier.to_lowercase()),
            GroupKey::Unresolved => "unresolved".to_string(),
        }
    }
}

/// Whether `key` is usable as a directory name and git branch component.
///
/// Equivalent to `^[a-z0-9][a-z0-9-]{0,62}$`, hand-rolled to avoid a regex
/// dependency. Rejecting anything else is what keeps `..`, `/`, whitespace and
/// case-only collisions out of paths and branch names.
fn is_valid_named_key(key: &str) -> bool {
    if key.is_empty() || key.len() > MAX_KEY_LEN {
        return false;
    }
    let mut chars = key.chars();
    let first = chars.next().expect("non-empty");
    if !first.is_ascii_lowercase() && !first.is_ascii_digit() {
        return false;
    }
    chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

/// Derives the group key for an event.
///
/// Pure apart from logging: a malformed or ambiguous label is reported and then
/// falls back to a per-issue group, so a mislabelled ticket still gets worked
/// rather than being dropped.
pub fn group_key_for(event: &InboundEvent) -> GroupKey {
    match event {
        InboundEvent::LinearIssueTodo { issue, .. } => {
            let fallback = || GroupKey::Issue(issue.identifier.clone());
            match issue.session_labels.as_slice() {
                [] => fallback(),
                [only] => {
                    let lowered = only.to_lowercase();
                    if is_valid_named_key(&lowered) {
                        GroupKey::Named(lowered)
                    } else {
                        tracing::warn!(
                            issue = %issue.identifier,
                            label = %only,
                            "session label is not a valid group key \
                             (expected ^[a-z0-9][a-z0-9-]{{0,62}}$); \
                             falling back to a per-issue session"
                        );
                        fallback()
                    }
                }
                many => {
                    tracing::warn!(
                        issue = %issue.identifier,
                        labels = ?many,
                        "issue carries several session labels; \
                         falling back to a per-issue session"
                    );
                    fallback()
                }
            }
        }
        InboundEvent::GithubPrComment { .. } => GroupKey::Unresolved,
    }
}

// ── records ──────────────────────────────────────────────────────────────────

/// What a group is doing right now.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum GroupState {
    /// No process running.
    Idle,
    /// A dispatched `claude` run is in progress.
    Running {
        pid: u32,
        /// Start time *of the process*, used to detect PID reuse.
        started_at: DateTime<Utc>,
    },
    /// An operator has attached to the session.
    Attached { pid: u32, started_at: DateTime<Utc> },
}

impl GroupState {
    /// The process backing this state, if any.
    fn process(&self) -> Option<(u32, DateTime<Utc>)> {
        match self {
            GroupState::Idle => None,
            GroupState::Running { pid, started_at } | GroupState::Attached { pid, started_at } => {
                Some((*pid, *started_at))
            }
        }
    }
}

/// How a run ended.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "result", rename_all = "snake_case")]
pub enum RunResult {
    Exited {
        code: i32,
    },
    Signaled {
        signal: i32,
    },
    TimedOut,
    Killed {
        by: String,
    },
    Interrupted,
    /// The process vanished without an observed exit — typically a pod restart.
    Lost,
}

/// The outcome of the most recent run.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RunOutcome {
    #[serde(flatten)]
    pub result: RunResult,
    pub finished_at: DateTime<Utc>,
    pub duration: Duration,
}

/// An accepted event waiting to be dispatched.
///
/// Queued events live in the registry so a webhook that already received `200`
/// survives a pod restart.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct QueuedEvent {
    pub event: InboundEvent,
    pub received_at: DateTime<Utc>,
}

/// Everything known about one session group.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Group {
    pub key: GroupKey,
    /// Passed to `claude --session-id` on a fresh run, `--resume` thereafter.
    pub session_id: Uuid,
    /// False until a run has actually used `session_id`; resuming before then
    /// would fail, since the session does not exist yet.
    pub session_started: bool,
    pub state: GroupState,
    /// Empty until the dispatcher (NEX-128) creates the worktree.
    pub worktree: PathBuf,
    pub branches: BTreeSet<String>,
    pub issues: Vec<String>,
    pub pending: VecDeque<QueuedEvent>,
    pub last_active: DateTime<Utc>,
    pub last_run: Option<RunOutcome>,
    pub created_at: DateTime<Utc>,
}

// ── process liveness ─────────────────────────────────────────────────────────

/// Whether a PID exists at all, ignoring identity.
fn pid_exists(pid: u32) -> bool {
    use nix::errno::Errno;
    use nix::sys::signal::kill;
    use nix::unistd::Pid;

    // `kill` treats 0 and negative pids as "a process group", not "a process",
    // so they must never reach it: pid 0 would report a dead run as alive.
    if pid == 0 || pid > i32::MAX as u32 {
        return false;
    }

    match kill(Pid::from_raw(pid as i32), None) {
        Ok(()) => true,
        // Alive but owned by another user.
        Err(Errno::EPERM) => true,
        Err(_) => false,
    }
}

#[cfg(any(target_os = "linux", test))]
/// Extracts field 22 (`starttime`, in clock ticks since boot) from the contents
/// of `/proc/<pid>/stat`.
///
/// Field 2 is the executable name in parentheses and may itself contain spaces
/// and parentheses, so parsing starts after the *last* `)`.
fn parse_starttime_ticks(stat: &str) -> Option<u64> {
    let after_comm = stat.rsplit_once(')')?.1;
    // The remainder begins at field 3, so field 22 is index 19.
    after_comm.split_whitespace().nth(19)?.parse().ok()
}

/// Seconds since the epoch at which the system booted, from `/proc/stat`.
#[cfg(target_os = "linux")]
fn boot_time_secs() -> Option<i64> {
    let stat = std::fs::read_to_string("/proc/stat").ok()?;
    stat.lines()
        .find_map(|l| l.strip_prefix("btime "))?
        .trim()
        .parse()
        .ok()
}

/// Wall-clock start time of a running process.
#[cfg(target_os = "linux")]
fn process_start_time(pid: u32) -> Option<DateTime<Utc>> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let ticks = parse_starttime_ticks(&stat)?;
    let hz = nix::unistd::sysconf(nix::unistd::SysconfVar::CLK_TCK)
        .ok()
        .flatten()
        .filter(|v| *v > 0)? as u64;
    let boot = boot_time_secs()?;
    use chrono::TimeZone;
    Utc.timestamp_opt(boot + (ticks / hz) as i64, 0).single()
}

/// Whether the process behind a recorded PID is still the *same* process.
///
/// A bare `kill(pid, 0)` is not enough after a pod restart: every PID from the
/// previous container is gone and the kernel hands the low numbers straight back
/// out, so a stale record can easily point at an unrelated live process. On Linux
/// the recorded start time is therefore compared with the live one.
#[cfg(target_os = "linux")]
fn process_alive(pid: u32, recorded_start: DateTime<Utc>) -> bool {
    if !pid_exists(pid) {
        return false;
    }
    match process_start_time(pid) {
        Some(actual) => (actual - recorded_start)
            .to_std()
            .or_else(|_| (recorded_start - actual).to_std())
            .map(|d| d <= START_TIME_TOLERANCE)
            .unwrap_or(false),
        // Exists but unreadable: treat as alive rather than kill a real run.
        None => true,
    }
}

/// Fallback for local development on macOS.
///
/// `/proc` does not exist here, so PID reuse cannot be detected and a recycled
/// PID will be reported as alive. Acceptable because production is Linux and a
/// developer machine does not restart containers underneath the registry.
#[cfg(not(target_os = "linux"))]
fn process_alive(pid: u32, _recorded_start: DateTime<Utc>) -> bool {
    pid_exists(pid)
}

// ── registry ─────────────────────────────────────────────────────────────────

/// Parses `CONDUCTOR_IDLE_DAYS`, falling back to the default.
///
/// A value that is absent, unparseable or non-positive falls back rather than
/// erroring: a malformed setting should not stop the service starting, and a
/// zero or negative window would expire every session on every dispatch.
fn parse_idle_days(raw: Option<&str>) -> i64 {
    raw.and_then(|v| v.trim().parse::<i64>().ok())
        .filter(|d| *d > 0)
        .unwrap_or(DEFAULT_IDLE_DAYS)
}

/// Handle to the Postgres-backed registry.
#[derive(Clone)]
pub struct Registry {
    pool: PgPool,
    clock: Arc<dyn Clock>,
    idle_days: i64,
}

impl Registry {
    /// Wraps an existing pool.
    pub fn new(pool: PgPool, clock: Arc<dyn Clock>, idle_days: i64) -> Self {
        Registry {
            pool,
            clock,
            idle_days,
        }
    }

    /// Connects using `DATABASE_URL`, with the idle window from
    /// `CONDUCTOR_IDLE_DAYS`.
    pub async fn from_env(clock: Arc<dyn Clock>) -> Result<Self, sqlx::Error> {
        let url = std::env::var(DATABASE_URL_ENV).map_err(|_| {
            sqlx::Error::Configuration(format!("{DATABASE_URL_ENV} is not set").into())
        })?;
        let idle_days = parse_idle_days(std::env::var(IDLE_DAYS_ENV).ok().as_deref());
        let pool = PgPool::connect(&url).await?;
        Ok(Registry::new(pool, clock, idle_days))
    }

    /// Applies the embedded migrations.
    pub async fn migrate(&self) -> Result<(), sqlx::migrate::MigrateError> {
        sqlx::migrate!("./migrations").run(&self.pool).await
    }

    /// Begins a transaction holding the registry-wide advisory lock.
    ///
    /// The lock is released when the transaction ends, committed or not, so a
    /// panicking caller cannot wedge the registry.
    pub async fn begin(&self) -> Result<RegistryTxn<'_>, sqlx::Error> {
        let mut tx = self.pool.begin().await?;
        sqlx::query("SELECT pg_advisory_xact_lock($1)")
            .bind(REGISTRY_LOCK_KEY)
            .execute(&mut *tx)
            .await?;
        Ok(RegistryTxn {
            tx,
            now: self.clock.now(),
            idle_days: self.idle_days,
        })
    }

    /// Convenience wrapper: one locked transaction around a single dispatch.
    pub async fn session_for_dispatch(&self, key: &GroupKey) -> Result<(Uuid, bool), sqlx::Error> {
        let mut txn = self.begin().await?;
        let out = txn.session_for_dispatch(key).await?;
        txn.commit().await?;
        Ok(out)
    }

    /// Convenience wrapper: one locked transaction around a single dedup check.
    pub async fn mark_delivery_seen(&self, delivery_id: Option<&str>) -> Result<bool, sqlx::Error> {
        let mut txn = self.begin().await?;
        let out = txn.mark_delivery_seen(delivery_id).await?;
        txn.commit().await?;
        Ok(out)
    }

    /// Every group, with its pending queue, ordered by slug.
    ///
    /// Read-only and unlocked: callers get a consistent snapshot from the
    /// transaction, not a guarantee that nothing changes afterwards.
    pub async fn snapshot(&self) -> Result<Vec<Group>, sqlx::Error> {
        let mut tx = self.pool.begin().await?;
        let rows = sqlx::query("SELECT * FROM session_groups ORDER BY slug")
            .fetch_all(&mut *tx)
            .await?;

        let mut groups = Vec::with_capacity(rows.len());
        for row in &rows {
            let slug: String = row.try_get("slug")?;
            let pending = sqlx::query(
                "SELECT event, received_at FROM pending_events \
                 WHERE group_slug = $1 ORDER BY id",
            )
            .bind(&slug)
            .fetch_all(&mut *tx)
            .await?;
            groups.push(group_from_row(row, pending_from_rows(&pending)?)?);
        }
        tx.commit().await?;
        Ok(groups)
    }

    /// Resets groups whose recorded process is gone, returning how many.
    ///
    /// After a restart this is the normal path, not an edge case: every PID from
    /// the previous container has died.
    pub async fn reconcile_stale_processes(&self) -> Result<usize, sqlx::Error> {
        let mut txn = self.begin().await?;
        let n = txn.reconcile_stale_processes().await?;
        txn.commit().await?;
        Ok(n)
    }
}

/// Rebuilds the queue for one group.
fn pending_from_rows(rows: &[PgRow]) -> Result<VecDeque<QueuedEvent>, sqlx::Error> {
    rows.iter()
        .map(|r| {
            Ok(QueuedEvent {
                event: r.try_get::<Json<InboundEvent>, _>("event")?.0,
                received_at: r.try_get("received_at")?,
            })
        })
        .collect()
}

/// Rebuilds a [`Group`] from its row.
fn group_from_row(row: &PgRow, pending: VecDeque<QueuedEvent>) -> Result<Group, sqlx::Error> {
    Ok(Group {
        key: row.try_get::<Json<GroupKey>, _>("key")?.0,
        session_id: row.try_get("session_id")?,
        session_started: row.try_get("session_started")?,
        state: row.try_get::<Json<GroupState>, _>("state")?.0,
        worktree: PathBuf::from(row.try_get::<String, _>("worktree")?),
        branches: row
            .try_get::<Vec<String>, _>("branches")?
            .into_iter()
            .collect(),
        issues: row.try_get("issues")?,
        pending,
        last_active: row.try_get("last_active")?,
        last_run: row
            .try_get::<Option<Json<RunOutcome>>, _>("last_run")?
            .map(|j| j.0),
        created_at: row.try_get("created_at")?,
    })
}

/// A transaction against the registry, holding the advisory lock.
///
/// Carries the current time so callers cannot mix clocks, which is what lets
/// expiry be tested without sleeping. Dropping without [`RegistryTxn::commit`]
/// rolls everything back.
pub struct RegistryTxn<'a> {
    tx: Transaction<'a, Postgres>,
    now: DateTime<Utc>,
    idle_days: i64,
}

impl RegistryTxn<'_> {
    /// The time this transaction is operating at.
    pub fn now(&self) -> DateTime<Utc> {
        self.now
    }

    /// Commits, releasing the advisory lock.
    pub async fn commit(self) -> Result<(), sqlx::Error> {
        self.tx.commit().await
    }

    /// Returns the session to dispatch into, and whether it can be resumed.
    ///
    /// A session is resumable only once a run has actually started it and while
    /// the group has been active inside the idle window. Otherwise a fresh UUID
    /// is minted — but the group itself is kept, along with its worktree,
    /// branches and issue history, so expiry costs the conversation and nothing
    /// else.
    pub async fn session_for_dispatch(
        &mut self,
        key: &GroupKey,
    ) -> Result<(Uuid, bool), sqlx::Error> {
        let slug = key.slug();
        let existing = sqlx::query(
            "SELECT session_id, session_started, last_active \
             FROM session_groups WHERE slug = $1",
        )
        .bind(&slug)
        .fetch_optional(&mut *self.tx)
        .await?;

        let Some(row) = existing else {
            let session_id = Uuid::new_v4();
            sqlx::query(
                "INSERT INTO session_groups \
                   (slug, key, session_id, session_started, state, last_active, created_at) \
                 VALUES ($1, $2, $3, FALSE, $4, $5, $5)",
            )
            .bind(&slug)
            .bind(Json(key))
            .bind(session_id)
            .bind(Json(GroupState::Idle))
            .bind(self.now)
            .execute(&mut *self.tx)
            .await?;
            return Ok((session_id, false));
        };

        let session_id: Uuid = row.try_get("session_id")?;
        let session_started: bool = row.try_get("session_started")?;
        let last_active: DateTime<Utc> = row.try_get("last_active")?;

        let idle_for = self.now - last_active;
        let resumable = session_started && idle_for < chrono::Duration::days(self.idle_days);

        if resumable {
            // Touched after the comparison, so dispatching cannot retroactively
            // keep a group alive.
            sqlx::query("UPDATE session_groups SET last_active = $2 WHERE slug = $1")
                .bind(&slug)
                .bind(self.now)
                .execute(&mut *self.tx)
                .await?;
            return Ok((session_id, true));
        }

        if session_started {
            tracing::info!(
                group = %slug,
                idle_days = idle_for.num_days(),
                "session idle past the expiry window; starting a new one"
            );
        }
        let fresh = Uuid::new_v4();
        sqlx::query(
            "UPDATE session_groups \
             SET session_id = $2, session_started = FALSE, last_active = $3 \
             WHERE slug = $1",
        )
        .bind(&slug)
        .bind(fresh)
        .bind(self.now)
        .execute(&mut *self.tx)
        .await?;
        Ok((fresh, false))
    }

    /// Records a delivery ID, returning whether it is new.
    ///
    /// `None` is always new: without an ID there is nothing to deduplicate on,
    /// and dropping such an event would lose work.
    pub async fn mark_delivery_seen(
        &mut self,
        delivery_id: Option<&str>,
    ) -> Result<bool, sqlx::Error> {
        let Some(id) = delivery_id else {
            return Ok(true);
        };

        let inserted = sqlx::query(
            "INSERT INTO seen_deliveries (delivery_id, seen_at) VALUES ($1, $2) \
             ON CONFLICT (delivery_id) DO NOTHING \
             RETURNING delivery_id",
        )
        .bind(id)
        .bind(self.now)
        .fetch_optional(&mut *self.tx)
        .await?
        .is_some();

        if inserted {
            // Cap the table, oldest first.
            sqlx::query(
                "DELETE FROM seen_deliveries \
                 WHERE seq <= (SELECT MAX(seq) FROM seen_deliveries) - $1",
            )
            .bind(MAX_SEEN_DELIVERIES as i64)
            .execute(&mut *self.tx)
            .await?;
        }

        Ok(inserted)
    }

    /// Marks this group's session as started, so later dispatches resume it.
    pub async fn mark_session_started(&mut self, key: &GroupKey) -> Result<(), sqlx::Error> {
        sqlx::query("UPDATE session_groups SET session_started = TRUE WHERE slug = $1")
            .bind(key.slug())
            .execute(&mut *self.tx)
            .await?;
        Ok(())
    }

    /// Queues an accepted event for later dispatch.
    pub async fn enqueue(
        &mut self,
        key: &GroupKey,
        event: &InboundEvent,
    ) -> Result<(), sqlx::Error> {
        sqlx::query(
            "INSERT INTO pending_events (group_slug, event, received_at) VALUES ($1, $2, $3)",
        )
        .bind(key.slug())
        .bind(Json(event))
        .bind(self.now)
        .execute(&mut *self.tx)
        .await?;
        Ok(())
    }

    /// Appends a Linear identifier to the group's history.
    ///
    /// Deliberately a read-modify-write rather than `array_append`: this is the
    /// shape the dispatcher needs, and it is exactly what the advisory lock
    /// protects. Without the lock two transactions would both read the old
    /// array and the second would overwrite the first's entry.
    pub async fn append_issue(&mut self, key: &GroupKey, issue: &str) -> Result<(), sqlx::Error> {
        let slug = key.slug();
        let mut issues: Vec<String> =
            sqlx::query_scalar("SELECT issues FROM session_groups WHERE slug = $1")
                .bind(&slug)
                .fetch_one(&mut *self.tx)
                .await?;
        issues.push(issue.to_string());
        sqlx::query("UPDATE session_groups SET issues = $2 WHERE slug = $1")
            .bind(&slug)
            .bind(&issues)
            .execute(&mut *self.tx)
            .await?;
        Ok(())
    }

    /// Groups ready to run, oldest queued event first.
    ///
    /// Runnable means `Idle` with something pending. Ordering by the oldest
    /// pending event keeps a busy group from starving one that has waited
    /// longer.
    pub async fn runnable_groups(&mut self) -> Result<Vec<GroupKey>, sqlx::Error> {
        let rows = sqlx::query(
            "SELECT g.key AS key \
             FROM session_groups g \
             JOIN (SELECT group_slug, MIN(received_at) AS oldest \
                     FROM pending_events GROUP BY group_slug) p \
               ON p.group_slug = g.slug \
             WHERE g.state->>'state' = 'idle' \
             ORDER BY p.oldest",
        )
        .fetch_all(&mut *self.tx)
        .await?;

        rows.iter()
            .map(|r| Ok(r.try_get::<Json<GroupKey>, _>("key")?.0))
            .collect()
    }

    /// Claims a group for a run: verifies it is `Idle`, pops its oldest pending
    /// event and marks it `Running`.
    ///
    /// All three happen under the transaction's lock, which is what guarantees
    /// a group never has two runs — including against the CLI in another
    /// process. Returns `None` if the group is busy or has nothing queued.
    ///
    /// `owner_pid` is the dispatcher's own pid, recorded until the child exists.
    /// If the dispatcher dies in that window, stale-PID recovery sees its pid is
    /// gone and marks the run `Lost`, which is the correct outcome.
    pub async fn claim_next(
        &mut self,
        key: &GroupKey,
        owner_pid: u32,
        owner_started_at: DateTime<Utc>,
    ) -> Result<Option<QueuedEvent>, sqlx::Error> {
        let slug = key.slug();

        let state = sqlx::query_scalar::<_, Json<GroupState>>(
            "SELECT state FROM session_groups WHERE slug = $1 FOR UPDATE",
        )
        .bind(&slug)
        .fetch_optional(&mut *self.tx)
        .await?;

        match state {
            Some(Json(GroupState::Idle)) => {}
            // Busy or absent: leave the queue untouched.
            _ => return Ok(None),
        }

        let popped = sqlx::query(
            "DELETE FROM pending_events \
             WHERE id = (SELECT id FROM pending_events WHERE group_slug = $1 \
                          ORDER BY id LIMIT 1) \
             RETURNING event, received_at",
        )
        .bind(&slug)
        .fetch_optional(&mut *self.tx)
        .await?;

        let Some(row) = popped else {
            return Ok(None);
        };

        sqlx::query("UPDATE session_groups SET state = $2 WHERE slug = $1")
            .bind(&slug)
            .bind(Json(GroupState::Running {
                pid: owner_pid,
                started_at: owner_started_at,
            }))
            .execute(&mut *self.tx)
            .await?;

        Ok(Some(QueuedEvent {
            event: row.try_get::<Json<InboundEvent>, _>("event")?.0,
            received_at: row.try_get("received_at")?,
        }))
    }

    /// Replaces the recorded process once the child has spawned.
    pub async fn set_running_process(
        &mut self,
        key: &GroupKey,
        pid: u32,
        started_at: DateTime<Utc>,
    ) -> Result<(), sqlx::Error> {
        sqlx::query("UPDATE session_groups SET state = $2 WHERE slug = $1")
            .bind(key.slug())
            .bind(Json(GroupState::Running { pid, started_at }))
            .execute(&mut *self.tx)
            .await?;
        Ok(())
    }

    /// Returns a group to `Idle` and records how its run ended.
    pub async fn finish_run(
        &mut self,
        key: &GroupKey,
        outcome: &RunOutcome,
    ) -> Result<(), sqlx::Error> {
        sqlx::query(
            "UPDATE session_groups \
             SET state = $2, last_run = $3, last_active = $4 \
             WHERE slug = $1",
        )
        .bind(key.slug())
        .bind(Json(GroupState::Idle))
        .bind(Json(outcome))
        .bind(self.now)
        .execute(&mut *self.tx)
        .await?;
        Ok(())
    }

    /// Records the worktree backing a group.
    pub async fn set_worktree(
        &mut self,
        key: &GroupKey,
        worktree: &std::path::Path,
    ) -> Result<(), sqlx::Error> {
        sqlx::query("UPDATE session_groups SET worktree = $2 WHERE slug = $1")
            .bind(key.slug())
            .bind(worktree.to_string_lossy().to_string())
            .execute(&mut *self.tx)
            .await?;
        Ok(())
    }

    /// Notes a branch seen checked out in the group's worktree.
    ///
    /// Every branch is kept, not just the current one: the agent may open a PR
    /// from a branch it created, and routing that PR's comments back here later
    /// depends on having seen it.
    pub async fn record_branch(&mut self, key: &GroupKey, branch: &str) -> Result<(), sqlx::Error> {
        let slug = key.slug();
        let mut branches: Vec<String> =
            sqlx::query_scalar("SELECT branches FROM session_groups WHERE slug = $1")
                .bind(&slug)
                .fetch_one(&mut *self.tx)
                .await?;
        if branches.iter().any(|b| b == branch) {
            return Ok(());
        }
        branches.push(branch.to_string());
        branches.sort();
        sqlx::query("UPDATE session_groups SET branches = $2 WHERE slug = $1")
            .bind(&slug)
            .bind(&branches)
            .execute(&mut *self.tx)
            .await?;
        Ok(())
    }

    /// Resets groups whose recorded process is gone, returning how many.
    pub async fn reconcile_stale_processes(&mut self) -> Result<usize, sqlx::Error> {
        let rows = sqlx::query("SELECT slug, state FROM session_groups")
            .fetch_all(&mut *self.tx)
            .await?;

        let mut reset = 0usize;
        for row in rows {
            let slug: String = row.try_get("slug")?;
            let state = row.try_get::<Json<GroupState>, _>("state")?.0;
            let Some((pid, started_at)) = state.process() else {
                continue;
            };
            if process_alive(pid, started_at) {
                continue;
            }

            tracing::warn!(group = %slug, pid, "recorded process is gone; marking the run lost");
            let outcome = RunOutcome {
                result: RunResult::Lost,
                finished_at: self.now,
                duration: (self.now - started_at).to_std().unwrap_or_default(),
            };
            sqlx::query("UPDATE session_groups SET state = $2, last_run = $3 WHERE slug = $1")
                .bind(&slug)
                .bind(Json(GroupState::Idle))
                .bind(Json(&outcome))
                .execute(&mut *self.tx)
                .await?;
            reset += 1;
        }
        Ok(reset)
    }
}

/// Helpers shared by both test modules.
#[cfg(test)]
mod tests_support {
    use super::*;
    use crate::linear::LinearIssue;
    use std::sync::Mutex;

    /// Clock the tests advance by hand, so expiry needs no sleeping.
    pub struct FakeClock(Mutex<DateTime<Utc>>);

    impl FakeClock {
        pub fn new(at: DateTime<Utc>) -> Arc<Self> {
            Arc::new(FakeClock(Mutex::new(at)))
        }
        pub fn advance(&self, by: chrono::Duration) {
            *self.0.lock().expect("clock") += by;
        }
    }

    impl Clock for FakeClock {
        fn now(&self) -> DateTime<Utc> {
            *self.0.lock().expect("clock")
        }
    }

    /// Fixed instant the tests measure from.
    pub fn epoch() -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-01-01T00:00:00Z")
            .expect("valid")
            .with_timezone(&Utc)
    }

    /// A Linear event carrying the given session labels.
    pub fn linear_event(identifier: &str, session_labels: &[&str]) -> InboundEvent {
        InboundEvent::LinearIssueTodo {
            issue: LinearIssue {
                identifier: identifier.to_string(),
                title: "t".into(),
                description: "d".into(),
                labels: session_labels.iter().map(|s| s.to_string()).collect(),
                session_labels: session_labels.iter().map(|s| s.to_string()).collect(),
            },
            delivery_id: None,
        }
    }

    /// Runs a trivial child to completion and returns its now-dead pid.
    pub fn reaped_pid() -> u32 {
        let mut child = std::process::Command::new("/bin/sh")
            .args(["-c", "exit 0"])
            .spawn()
            .expect("spawn");
        let pid = child.id();
        child.wait().expect("wait");
        pid
    }

    /// This process's real start time.
    ///
    /// On Linux it must match `/proc` exactly or liveness checks reject it. On
    /// macOS there is no start time to compare, so any value serves.
    pub fn own_start_time() -> DateTime<Utc> {
        #[cfg(target_os = "linux")]
        {
            process_start_time(std::process::id()).expect("own start time from /proc")
        }
        #[cfg(not(target_os = "linux"))]
        {
            epoch()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::tests_support::*;
    use super::*;
    use crate::github::{GithubPrComment, PrCommentKind};

    // ── key derivation (no database) ─────────────────────────────────────────

    #[test]
    fn single_session_label_names_the_group() {
        assert_eq!(
            group_key_for(&linear_event("NEX-42", &["conductor-core"])),
            GroupKey::Named("conductor-core".into())
        );
    }

    #[test]
    fn no_session_label_gives_the_issue_its_own_group() {
        assert_eq!(
            group_key_for(&linear_event("NEX-42", &[])),
            GroupKey::Issue("NEX-42".into())
        );
    }

    #[test]
    fn several_session_labels_fall_back_to_the_issue() {
        assert_eq!(
            group_key_for(&linear_event("NEX-42", &["one", "two"])),
            GroupKey::Issue("NEX-42".into())
        );
    }

    #[test]
    fn invalid_named_key_falls_back_to_the_issue() {
        for bad in [
            "../escape",
            "has space",
            "has/slash",
            "-leading",
            &"x".repeat(64),
        ] {
            assert_eq!(
                group_key_for(&linear_event("NEX-42", &[bad])),
                GroupKey::Issue("NEX-42".into()),
                "{bad:?} should have been rejected"
            );
        }
    }

    #[test]
    fn named_keys_are_lowercased_so_case_cannot_collide() {
        assert_eq!(
            group_key_for(&linear_event("NEX-42", &["Conductor-Core"])),
            GroupKey::Named("conductor-core".into())
        );
    }

    #[test]
    fn github_comments_are_unresolved() {
        let event = InboundEvent::GithubPrComment {
            comment: GithubPrComment {
                repo: "lassemand/nexus".into(),
                pr_number: 1,
                comment_id: 2,
                body: "b".into(),
                file_path: None,
            },
            kind: PrCommentKind::Comment,
            delivery_id: None,
        };
        assert_eq!(group_key_for(&event), GroupKey::Unresolved);
    }

    #[test]
    fn slugs_are_path_safe() {
        assert_eq!(
            GroupKey::Named("conductor-core".into()).slug(),
            "conductor-core"
        );
        assert_eq!(GroupKey::Issue("NEX-42".into()).slug(), "linear-nex-42");
    }

    #[test]
    fn key_validation_boundaries() {
        assert!(is_valid_named_key("a"));
        assert!(is_valid_named_key("0"));
        assert!(is_valid_named_key(&"a".repeat(MAX_KEY_LEN)));
        assert!(!is_valid_named_key(&"a".repeat(MAX_KEY_LEN + 1)));
        assert!(!is_valid_named_key(""));
        assert!(!is_valid_named_key("-lead"));
        assert!(!is_valid_named_key("UPPER"));
        assert!(!is_valid_named_key("under_score"));
        assert!(!is_valid_named_key(".."));
    }

    // ── process liveness (no database) ───────────────────────────────────────

    #[test]
    fn starttime_is_parsed_past_a_comm_containing_spaces_and_parens() {
        // Field 2 is "(weird )name)" here — parsing must start after the LAST ')'.
        // Values are numbered to match their field position: field 3 holds "3".
        let fields: Vec<String> = (3..=52).map(|i| i.to_string()).collect();
        let stat = format!("1234 (weird )name) {}", fields.join(" "));
        assert_eq!(parse_starttime_ticks(&stat), Some(22));
        assert_eq!(parse_starttime_ticks("garbage"), None);
    }

    #[test]
    fn a_dead_pid_is_not_alive() {
        assert!(!process_alive(reaped_pid(), epoch()));
    }

    #[test]
    fn pid_zero_is_never_treated_as_alive() {
        // kill(0, 0) addresses the caller's process group and succeeds, so the
        // guard in pid_exists is what stops a dead run looking alive forever.
        assert!(!process_alive(0, epoch()));
    }

    // ── storage (needs Postgres; CI provides DATABASE_URL) ───────────────────

    fn registry(pool: PgPool, clock: Arc<FakeClock>) -> Registry {
        Registry::new(pool, clock, DEFAULT_IDLE_DAYS)
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn first_dispatch_starts_a_session_and_later_ones_resume_it(pool: PgPool) {
        let clock = FakeClock::new(epoch());
        let reg = registry(pool, clock.clone());
        let key = GroupKey::Named("grp".into());

        let mut txn = reg.begin().await.expect("begin");
        let (first, resume) = txn.session_for_dispatch(&key).await.expect("dispatch");
        assert!(!resume, "a brand new session cannot be resumed");
        // The dispatcher marks this once a run has really used the id.
        txn.mark_session_started(&key).await.expect("started");
        txn.commit().await.expect("commit");

        clock.advance(chrono::Duration::days(DEFAULT_IDLE_DAYS - 1));
        let (second, resume) = reg.session_for_dispatch(&key).await.expect("dispatch");
        assert!(resume, "still inside the idle window");
        assert_eq!(first, second, "the same session is reused");
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn session_expires_after_the_idle_window_but_the_group_survives(pool: PgPool) {
        let clock = FakeClock::new(epoch());
        let reg = registry(pool.clone(), clock.clone());
        let key = GroupKey::Named("grp".into());

        let mut txn = reg.begin().await.expect("begin");
        let (first, _) = txn.session_for_dispatch(&key).await.expect("dispatch");
        txn.mark_session_started(&key).await.expect("started");
        txn.commit().await.expect("commit");

        // History the group accumulates and must not lose on expiry.
        sqlx::query(
            "UPDATE session_groups SET worktree = $2, branches = $3, issues = $4 WHERE slug = $1",
        )
        .bind(key.slug())
        .bind("/data/worktrees/grp")
        .bind(vec!["feature/NEX-1".to_string()])
        .bind(vec!["NEX-1".to_string()])
        .execute(&pool)
        .await
        .expect("seed history");

        clock.advance(chrono::Duration::days(DEFAULT_IDLE_DAYS + 1));
        let (second, resume) = reg.session_for_dispatch(&key).await.expect("dispatch");
        assert!(!resume, "past the window the session is not resumable");
        assert_ne!(first, second, "a fresh session id is minted");

        let groups = reg.snapshot().await.expect("snapshot");
        let group = groups.first().expect("group survived");
        assert!(!group.session_started);
        assert_eq!(group.worktree, PathBuf::from("/data/worktrees/grp"));
        assert!(group.branches.contains("feature/NEX-1"));
        assert_eq!(group.issues, vec!["NEX-1".to_string()]);
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn unstarted_session_is_never_resumed_however_recent(pool: PgPool) {
        let reg = registry(pool, FakeClock::new(epoch()));
        let key = GroupKey::Named("grp".into());

        reg.session_for_dispatch(&key).await.expect("dispatch");
        // No run has used the id, so resuming it would fail.
        let (_, resume) = reg.session_for_dispatch(&key).await.expect("dispatch");
        assert!(!resume);
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn delivery_ids_are_deduplicated_and_survive_restart(pool: PgPool) {
        let reg = registry(pool, FakeClock::new(epoch()));

        assert!(reg.mark_delivery_seen(Some("a")).await.expect("mark"));
        assert!(!reg.mark_delivery_seen(Some("a")).await.expect("mark"));
        // Without an ID there is nothing to dedup on.
        assert!(reg.mark_delivery_seen(None).await.expect("mark"));
        assert!(reg.mark_delivery_seen(None).await.expect("mark"));
        // Still remembered by a separate handle, i.e. across a restart.
        assert!(!reg.mark_delivery_seen(Some("a")).await.expect("mark"));
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn seen_deliveries_are_capped_oldest_first(pool: PgPool) {
        let reg = registry(pool.clone(), FakeClock::new(epoch()));

        let mut txn = reg.begin().await.expect("begin");
        txn.mark_delivery_seen(Some("oldest")).await.expect("mark");
        for i in 0..MAX_SEEN_DELIVERIES {
            txn.mark_delivery_seen(Some(&format!("id-{i}")))
                .await
                .expect("mark");
        }
        txn.commit().await.expect("commit");

        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM seen_deliveries")
            .fetch_one(&pool)
            .await
            .expect("count");
        assert_eq!(count, MAX_SEEN_DELIVERIES as i64, "table is capped");
        // "oldest" was evicted, so it is treated as new again.
        assert!(reg.mark_delivery_seen(Some("oldest")).await.expect("mark"));
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn queued_events_round_trip(pool: PgPool) {
        let reg = registry(pool, FakeClock::new(epoch()));
        let key = GroupKey::Issue("NEX-7".into());
        let event = linear_event("NEX-7", &[]);

        let mut txn = reg.begin().await.expect("begin");
        txn.session_for_dispatch(&key).await.expect("dispatch");
        txn.enqueue(&key, &event).await.expect("enqueue");
        txn.commit().await.expect("commit");

        let groups = reg.snapshot().await.expect("snapshot");
        let group = groups.first().expect("group");
        assert_eq!(group.key, key);
        assert_eq!(group.pending.len(), 1);
        assert_eq!(
            group.pending[0].event, event,
            "the event survives serialisation"
        );
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn stale_running_group_is_reset_to_lost(pool: PgPool) {
        let clock = FakeClock::new(epoch());
        let reg = registry(pool.clone(), clock.clone());
        let key = GroupKey::Named("grp".into());
        let dead = reaped_pid();

        reg.session_for_dispatch(&key).await.expect("dispatch");
        sqlx::query("UPDATE session_groups SET state = $2 WHERE slug = $1")
            .bind(key.slug())
            .bind(Json(GroupState::Running {
                pid: dead,
                started_at: epoch(),
            }))
            .execute(&pool)
            .await
            .expect("seed state");

        clock.advance(chrono::Duration::seconds(30));
        assert_eq!(reg.reconcile_stale_processes().await.expect("reconcile"), 1);

        let groups = reg.snapshot().await.expect("snapshot");
        let group = groups.first().expect("group");
        assert_eq!(
            group.state,
            GroupState::Idle,
            "reset because the pid is gone"
        );
        let outcome = group.last_run.as_ref().expect("an outcome was recorded");
        assert_eq!(outcome.result, RunResult::Lost);
        assert_eq!(outcome.duration, Duration::from_secs(30));
    }
}

/// Additional coverage required by NEX-127.
///
/// Kept in a second module so the tests NEX-126 needed while developing stay
/// distinguishable from the verification matrix added afterwards.
#[cfg(test)]
mod verification {
    use super::tests_support::*;
    use super::*;
    use tracing_test::traced_test;

    // ── key derivation ───────────────────────────────────────────────────────

    #[test]
    #[traced_test]
    fn two_session_labels_warn_before_falling_back() {
        let key = group_key_for(&linear_event("NEX-42", &["alpha", "beta"]));
        assert_eq!(key, GroupKey::Issue("NEX-42".into()));
        assert!(
            logs_contain("several session labels"),
            "the ambiguity should be reported, not silently resolved"
        );
    }

    #[test]
    #[traced_test]
    fn invalid_label_warns_before_falling_back() {
        let key = group_key_for(&linear_event("NEX-42", &["a/b"]));
        assert_eq!(key, GroupKey::Issue("NEX-42".into()));
        assert!(logs_contain("not a valid group key"));
    }

    #[test]
    fn case_variants_resolve_to_one_key() {
        let upper = group_key_for(&linear_event("NEX-1", &["Foo"]));
        let lower = group_key_for(&linear_event("NEX-2", &["foo"]));
        assert_eq!(upper, lower, "case must not create a second group");
        assert_eq!(upper.slug(), lower.slug());
    }

    // ── expiry ───────────────────────────────────────────────────────────────

    #[sqlx::test(migrations = "./migrations")]
    async fn just_inside_the_window_resumes(pool: PgPool) {
        let clock = FakeClock::new(epoch());
        let reg = Registry::new(pool, clock.clone(), DEFAULT_IDLE_DAYS);
        let key = GroupKey::Named("grp".into());

        let mut txn = reg.begin().await.expect("begin");
        let (first, _) = txn.session_for_dispatch(&key).await.expect("dispatch");
        txn.mark_session_started(&key).await.expect("started");
        txn.commit().await.expect("commit");

        // 6d23h — one hour short of expiry.
        clock.advance(chrono::Duration::days(6) + chrono::Duration::hours(23));
        let (second, resume) = reg.session_for_dispatch(&key).await.expect("dispatch");
        assert!(resume, "6d23h idle is still inside a 7 day window");
        assert_eq!(first, second);
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn exactly_at_the_window_expires(pool: PgPool) {
        let clock = FakeClock::new(epoch());
        let reg = Registry::new(pool, clock.clone(), DEFAULT_IDLE_DAYS);
        let key = GroupKey::Named("grp".into());

        let mut txn = reg.begin().await.expect("begin");
        let (first, _) = txn.session_for_dispatch(&key).await.expect("dispatch");
        txn.mark_session_started(&key).await.expect("started");
        txn.commit().await.expect("commit");

        // Exactly 7d: the comparison is strict, so this is expired.
        clock.advance(chrono::Duration::days(DEFAULT_IDLE_DAYS));
        let (second, resume) = reg.session_for_dispatch(&key).await.expect("dispatch");
        assert!(!resume);
        assert_ne!(first, second);
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn a_shorter_idle_window_moves_the_boundary(pool: PgPool) {
        let clock = FakeClock::new(epoch());
        // What CONDUCTOR_IDLE_DAYS=1 configures.
        let reg = Registry::new(pool, clock.clone(), 1);
        let key = GroupKey::Named("grp".into());

        let mut txn = reg.begin().await.expect("begin");
        txn.session_for_dispatch(&key).await.expect("dispatch");
        txn.mark_session_started(&key).await.expect("started");
        txn.commit().await.expect("commit");

        clock.advance(chrono::Duration::hours(23));
        let (_, resume) = reg.session_for_dispatch(&key).await.expect("dispatch");
        assert!(resume, "23h is inside a 1 day window");

        clock.advance(chrono::Duration::days(1));
        let (_, resume) = reg.session_for_dispatch(&key).await.expect("dispatch");
        assert!(!resume, "past 1 day the session expires");
    }

    #[test]
    fn idle_days_parsing_falls_back_rather_than_failing() {
        assert_eq!(parse_idle_days(Some("1")), 1);
        assert_eq!(parse_idle_days(Some(" 3 ")), 3);
        assert_eq!(parse_idle_days(None), DEFAULT_IDLE_DAYS);
        assert_eq!(parse_idle_days(Some("")), DEFAULT_IDLE_DAYS);
        assert_eq!(parse_idle_days(Some("not-a-number")), DEFAULT_IDLE_DAYS);
        // Zero or negative would expire every session on every dispatch.
        assert_eq!(parse_idle_days(Some("0")), DEFAULT_IDLE_DAYS);
        assert_eq!(parse_idle_days(Some("-5")), DEFAULT_IDLE_DAYS);
    }

    // ── locking ──────────────────────────────────────────────────────────────

    #[sqlx::test(migrations = "./migrations")]
    async fn concurrent_writers_do_not_lose_updates(pool: PgPool) {
        let clock = FakeClock::new(epoch());
        let reg = Registry::new(pool, clock, DEFAULT_IDLE_DAYS);
        let key = GroupKey::Named("grp".into());
        reg.session_for_dispatch(&key).await.expect("create group");

        // Two writers on separate connections. append_issue is a
        // read-modify-write, so without the advisory lock the two would
        // interleave and lose entries.
        const PER_WRITER: usize = 200;
        let mut writers = Vec::new();
        for writer in 0..2 {
            let reg = reg.clone();
            let key = key.clone();
            writers.push(tokio::spawn(async move {
                for i in 0..PER_WRITER {
                    let mut txn = reg.begin().await.expect("begin");
                    txn.append_issue(&key, &format!("W{writer}-{i}"))
                        .await
                        .expect("append");
                    txn.commit().await.expect("commit");
                }
            }));
        }
        for w in writers {
            w.await.expect("writer finished");
        }

        let groups = reg.snapshot().await.expect("snapshot");
        let issues = &groups.first().expect("group").issues;
        assert_eq!(
            issues.len(),
            PER_WRITER * 2,
            "every append must survive; a short count means updates were lost"
        );
        // And no entry was duplicated or corrupted.
        let unique: std::collections::BTreeSet<_> = issues.iter().collect();
        assert_eq!(unique.len(), PER_WRITER * 2);
    }

    // ── unreadable stored data ───────────────────────────────────────────────

    #[sqlx::test(migrations = "./migrations")]
    async fn unparseable_stored_json_errors_rather_than_panicking(pool: PgPool) {
        let reg = Registry::new(pool.clone(), FakeClock::new(epoch()), DEFAULT_IDLE_DAYS);
        let key = GroupKey::Named("grp".into());
        reg.session_for_dispatch(&key).await.expect("create group");

        // A state object that is valid JSON but not a GroupState — the closest
        // analogue to the corrupt-file case now that there is no file.
        sqlx::query("UPDATE session_groups SET state = $2 WHERE slug = $1")
            .bind(key.slug())
            .bind(serde_json::json!({"state": "nonsense"}))
            .execute(&pool)
            .await
            .expect("seed bad state");

        // Must surface as an error, not unwind.
        assert!(
            reg.snapshot().await.is_err(),
            "unreadable stored data should be reported, not panic or be silently dropped"
        );
    }

    // ── stale PID recovery ───────────────────────────────────────────────────

    #[sqlx::test(migrations = "./migrations")]
    async fn a_live_process_with_matching_start_time_keeps_running(pool: PgPool) {
        let clock = FakeClock::new(epoch());
        let reg = Registry::new(pool.clone(), clock, DEFAULT_IDLE_DAYS);
        let key = GroupKey::Named("grp".into());
        reg.session_for_dispatch(&key).await.expect("create group");

        let running = GroupState::Running {
            pid: std::process::id(),
            started_at: own_start_time(),
        };
        sqlx::query("UPDATE session_groups SET state = $2 WHERE slug = $1")
            .bind(key.slug())
            .bind(Json(&running))
            .execute(&pool)
            .await
            .expect("seed state");

        assert_eq!(
            reg.reconcile_stale_processes().await.expect("reconcile"),
            0,
            "this process is alive, so its run must not be reaped"
        );
        let groups = reg.snapshot().await.expect("snapshot");
        assert_eq!(groups.first().expect("group").state, running);
    }

    /// PID reuse is only detectable where `/proc` exposes a start time.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_reused_pid_is_treated_as_dead() {
        let pid = std::process::id();
        let actual = process_start_time(pid).expect("own start time");

        assert!(process_alive(pid, actual), "the real start time matches");
        // Same live PID, start time from before a restart: a different process.
        assert!(
            !process_alive(pid, actual - chrono::Duration::seconds(60)),
            "a mismatched start time means the PID was recycled"
        );
    }
}
