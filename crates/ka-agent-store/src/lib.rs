//! SQLite persistence for Ka Agent (SEED.md §3.5–§3.8, §8.7): schema v1,
//! forward-only migrations, repository methods over the core domain types,
//! the audit hash chain, and the FTS5 episode index.
//!
//! Concurrency model: one connection behind a `parking_lot::Mutex`; every
//! method takes `&self` and performs short blocking work. Callers run these
//! from blocking contexts (the runtime never awaits inside this crate).
//!
//! Enum-to-column mapping: statuses are stored as their `#[serde(rename_all
//! = "snake_case")]` string forms (e.g. `SessionStatus::AwaitingApproval`
//! -> `awaiting_approval`), which is exactly what the schema CHECK
//! constraints accept.
//!
//! Repository methods live in sibling modules (`sessions`, `turns`,
//! `queue`, `outbox`, `approvals`, `audit`, `identities`, `candidates`,
//! `tasks`, `intents`), each an `impl Store` block split from this file
//! (moves only).

mod approvals;
mod audit;
mod candidates;
mod identities;
mod intents;
mod outbox;
mod queue;
mod sessions;
mod tasks;
mod turns;

use std::path::Path;

use ka_agent_core::{
    Approval, ApprovalId, ApprovalKind, ApprovalStatus, AuditRecord, MemoryId, OutboxChunk,
    OutboxStatus, QueueId, QueueItem, RunId, Session, SessionId, SessionStatus, Turn, TurnId,
    TurnStatus,
};
use parking_lot::Mutex;
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Store result: `Ok(T)` or a [`StoreError`].
pub type Result<T, E = StoreError> = std::result::Result<T, E>;

/// Errors from the store. `IllegalTransition` carries the table and the
/// rejected `from -> to` pair so callers can log exactly which state machine
/// refused the write.
#[derive(Debug, Error)]
pub enum StoreError {
    #[error("sqlite error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("migration {from} -> {to} failed: {source}")]
    Migration {
        from: usize,
        to: usize,
        #[source]
        source: rusqlite::Error,
    },
    #[error("database is newer than this binary (user_version {version})")]
    Newer { version: usize },
    #[error("illegal {what} transition {from} -> {to}")]
    IllegalTransition {
        what: &'static str,
        from: String,
        to: String,
    },
    #[error("not found: {0}")]
    NotFound(String),
    #[error("audit chain broken at seq {seq}: {why}")]
    BrokenAuditChain { seq: i64, why: &'static str },
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

/// Forward-only migrations, one per schema version. Entry `i` upgrades a
/// database at `user_version == i` to `i + 1`; each runs in its own
/// transaction and sets `PRAGMA user_version`.
const MIGRATIONS: &[&str] = &[
    include_str!("schema_v1.sql"),
    include_str!("schema_v2.sql"),
    include_str!("schema_v3.sql"),
    include_str!("schema_v4.sql"),
    include_str!("schema_v5.sql"),
    include_str!("schema_v6.sql"),
    include_str!("schema_v7.sql"),
];

/// One run of a background job (task runner, memory compaction, ...).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunRow {
    pub id: RunId,
    pub kind: String,
    pub ref_id: Option<String>,
    pub status: String,
    pub stop: Option<String>,
    pub error_class: Option<String>,
    pub error_message: Option<String>,
    pub cost_usd: Option<f64>,
    pub tokens: Option<u64>,
    pub started_ms: i64,
    pub ended_ms: Option<i64>,
}

fn row_to_run(r: &rusqlite::Row<'_>) -> rusqlite::Result<RunRow> {
    Ok(RunRow {
        id: RunId(r.get::<_, String>(0)?),
        kind: r.get(1)?,
        ref_id: r.get(2)?,
        status: r.get(3)?,
        stop: r.get(4)?,
        error_class: r.get(5)?,
        error_message: r.get(6)?,
        cost_usd: r.get(7)?,
        tokens: r.get::<_, Option<i64>>(8)?.map(|t| t.max(0) as u64),
        started_ms: r.get(9)?,
        ended_ms: r.get(10)?,
    })
}

/// One FTS hit from [`Store::search_episodes`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EpisodeHit {
    pub session: String,
    pub turn: String,
    pub role: String,
    pub snippet: String,
    pub at_ms: i64,
}

/// One pending memory candidate (`/memory review`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CandidateRow {
    pub id: MemoryId,
    pub source: String,
    pub text: String,
    pub provenance: Option<String>,
    pub created_ms: i64,
}

/// One channel identity linked to this instance's operator (SEED.md §8.1).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IdentityRow {
    pub channel: String,
    pub address: String,
    pub linked_ms: i64,
    pub via: String,
}

/// One external-write intent (SEED.md §7.3). Inserted before the API call,
/// updated after, so a crash mid-call is visible as `executing` (never auto-retried).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IntentRow {
    pub id: String,
    pub tool: String,
    pub args_hash: String,
    pub status: String,
    pub external_ref: Option<String>,
    pub summary: Option<String>,
    pub created_ms: i64,
    pub updated_ms: i64,
}

/// The SQLite-backed store. Not `Clone`; share via `Arc<Store>`.
pub struct Store {
    conn: Mutex<Connection>,
}

impl std::fmt::Debug for Store {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Store").finish_non_exhaustive()
    }
}

/// The SQLite-backed state of Ka Agent.
impl Store {
    /// Open (or create) the database at `path`, apply pragmas and migrations.
    /// Parent directories are created as needed.
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent)?;
        }
        Self::init(Connection::open(path)?)
    }

    /// A private in-memory store (tests, tools, throwaway state).
    pub fn open_in_memory() -> Result<Self> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(conn: Connection) -> Result<Self> {
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        conn.pragma_update(None, "busy_timeout", 5000)?;
        let store = Self {
            conn: Mutex::new(conn),
        };
        store.migrate()?;
        Ok(store)
    }

    fn migrate(&self) -> Result<()> {
        let mut conn = self.conn.lock();
        let version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        let version = version.max(0) as usize;
        if version > MIGRATIONS.len() {
            return Err(StoreError::Newer { version });
        }
        for (idx, sql) in MIGRATIONS.iter().enumerate().skip(version) {
            let to = idx + 1;
            let tx = conn.transaction()?;
            tx.execute_batch(sql)?;
            tx.pragma_update(None, "user_version", to as i64)?;
            tx.commit().map_err(|source| StoreError::Migration {
                from: idx,
                to,
                source,
            })?;
        }
        Ok(())
    }

    fn lock(&self) -> parking_lot::MutexGuard<'_, Connection> {
        self.conn.lock()
    }

    /// Current schema version (`PRAGMA user_version`).
    pub fn user_version(&self) -> Result<usize> {
        let conn = self.lock();
        let v: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        Ok(v.max(0) as usize)
    }

    /// Consistent snapshot backup via `VACUUM INTO`. The target must not
    /// already exist.
    pub fn backup_to(&self, path: &Path) -> Result<()> {
        let conn = self.lock();
        conn.execute("VACUUM INTO ?1", params![path.to_string_lossy()])?;
        Ok(())
    }

    // ------------------------------------------------------------------
    // meta
    // ------------------------------------------------------------------

    /// Read one key from the `meta` table (daily budget counters, etc.).
    pub fn meta_get(&self, key: &str) -> Result<Option<String>> {
        let conn = self.lock();
        conn.query_row("SELECT value FROM meta WHERE key = ?1", params![key], |r| {
            r.get(0)
        })
        .optional()
        .map_err(StoreError::from)
    }

    /// Write one key to the `meta` table (upsert).
    pub fn meta_set(&self, key: &str, value: &str) -> Result<()> {
        let conn = self.lock();
        conn.execute(
            "INSERT OR REPLACE INTO meta(key, value) VALUES (?1, ?2)",
            params![key, value],
        )?;
        Ok(())
    }

    /// Atomically clear a key only while it still holds `expected`
    /// (compare-and-swap): exactly one caller wins, which is what makes a
    /// one-shot token single-use under concurrent redemption.
    pub fn consume_meta(&self, key: &str, expected: &str) -> Result<bool> {
        let conn = self.lock();
        let changed = conn.execute(
            "UPDATE meta SET value = '' WHERE key = ?1 AND value = ?2",
            params![key, expected],
        )?;
        Ok(changed == 1)
    }
}

fn parse_session_status(s: &str) -> Option<SessionStatus> {
    serde_json::from_str(&format!("\"{s}\"")).ok()
}

/// Collect a `MappedRows` iterator into `Result<Vec<T>, StoreError>`.
fn collect_rows<T>(
    rows: rusqlite::MappedRows<'_, impl FnMut(&rusqlite::Row<'_>) -> rusqlite::Result<T>>,
) -> Result<Vec<T>> {
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

fn parse_turn_status(s: &str) -> Option<TurnStatus> {
    serde_json::from_str(&format!("\"{s}\"")).ok()
}

fn parse_approval_status(s: &str) -> Option<ApprovalStatus> {
    serde_json::from_str(&format!("\"{s}\"")).ok()
}

fn parse_outbox_status(s: &str) -> Option<OutboxStatus> {
    serde_json::from_str(&format!("\"{s}\"")).ok()
}

/// Snake-case storage form of a serde unit-enum status.
fn status_str<T: Serialize>(value: &T) -> String {
    // Unit variants with rename_all=snake_case serialize to a bare JSON
    // string; strip the quotes. Anything else is a programming error we
    // surface as an empty string (CHECK constraints reject it).
    serde_json::to_string(value)
        .map(|s| s.trim_matches('"').to_string())
        .unwrap_or_default()
}

fn row_to_session(r: &rusqlite::Row<'_>) -> rusqlite::Result<Session> {
    Ok(Session {
        id: SessionId(r.get::<_, String>(0)?),
        workspace: std::path::PathBuf::from(r.get::<_, String>(1)?),
        strand_id: r.get(2)?,
        strand_path: r.get::<_, Option<String>>(3)?.map(std::path::PathBuf::from),
        status: parse_session_status(&r.get::<_, String>(4)?).unwrap_or(SessionStatus::Degraded),
        created_ms: r.get(5)?,
        updated_ms: r.get(6)?,
    })
}

fn row_to_turn(r: &rusqlite::Row<'_>) -> rusqlite::Result<Turn> {
    Ok(Turn {
        id: TurnId(r.get::<_, String>(0)?),
        session: SessionId(r.get::<_, String>(1)?),
        inbound_text: r.get(2)?,
        status: parse_turn_status(&r.get::<_, String>(3)?).unwrap_or(TurnStatus::Interrupted),
        stop: r.get(4)?,
        error_class: r.get(5)?,
        error_message: r.get(6)?,
        cost_usd: r.get(7)?,
        tokens: r.get::<_, i64>(8)?.max(0) as u64,
        created_ms: r.get(9)?,
        updated_ms: r.get(10)?,
    })
}

fn row_to_approval(r: &rusqlite::Row<'_>) -> rusqlite::Result<Approval> {
    let kind_json: String = r.get(3)?;
    Ok(Approval {
        id: ApprovalId(r.get::<_, String>(0)?),
        session: SessionId(r.get::<_, String>(1)?),
        turn: TurnId(r.get::<_, String>(2)?),
        kind: serde_json::from_str(&kind_json).unwrap_or(ApprovalKind::Intent {
            tool: String::new(),
            args_summary: String::new(),
            effect: String::new(),
        }),
        status: parse_approval_status(&r.get::<_, String>(4)?).unwrap_or(ApprovalStatus::Expired),
        expires_at_ms: r.get(5)?,
        decided_at_ms: r.get(6)?,
        created_ms: r.get(7)?,
    })
}

fn task_status_str(s: &ka_agent_core::TaskStatus) -> &'static str {
    match s {
        ka_agent_core::TaskStatus::Queued => "queued",
        ka_agent_core::TaskStatus::Running => "running",
        ka_agent_core::TaskStatus::WaitingApproval => "waiting_approval",
        ka_agent_core::TaskStatus::Succeeded => "succeeded",
        ka_agent_core::TaskStatus::Failed => "failed",
        ka_agent_core::TaskStatus::Cancelled => "cancelled",
        ka_agent_core::TaskStatus::Interrupted => "interrupted",
        ka_agent_core::TaskStatus::Unknown => "unknown",
        ka_agent_core::TaskStatus::Missed => "missed",
    }
}

fn row_to_task(r: &rusqlite::Row<'_>) -> rusqlite::Result<ka_agent_core::Task> {
    let session: Option<String> = r.get(1)?;
    let status: String = r.get(4)?;
    let read_only: i64 = r.get(5)?;
    let status = match status.as_str() {
        "queued" => ka_agent_core::TaskStatus::Queued,
        "running" => ka_agent_core::TaskStatus::Running,
        "waiting_approval" => ka_agent_core::TaskStatus::WaitingApproval,
        "succeeded" => ka_agent_core::TaskStatus::Succeeded,
        "failed" => ka_agent_core::TaskStatus::Failed,
        "cancelled" => ka_agent_core::TaskStatus::Cancelled,
        "interrupted" => ka_agent_core::TaskStatus::Interrupted,
        "missed" => ka_agent_core::TaskStatus::Missed,
        _ => ka_agent_core::TaskStatus::Unknown,
    };
    Ok(ka_agent_core::Task {
        id: ka_agent_core::TaskId(r.get::<_, String>(0)?),
        session: session.map(ka_agent_core::SessionId),
        title: r.get(2)?,
        kind: r.get(3)?,
        status,
        read_only: read_only != 0,
        scheduled_at_ms: r.get(6)?,
        payload: r.get(7)?,
        created_ms: r.get(8)?,
        updated_ms: r.get(9)?,
    })
}

/// Shared test helpers for the sibling modules' test suites.
#[cfg(test)]
fn temp_path(tag: &str) -> std::path::PathBuf {
    let mut p = std::env::temp_dir();
    p.push(format!("ka-agent-store-{}-{}.db", tag, std::process::id()));
    let _ = std::fs::remove_file(&p);
    p
}

/// Shared test helper: a session over a scratch workspace.
#[cfg(test)]
fn session() -> Session {
    Session::new(std::path::PathBuf::from("/tmp/ws"))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[test]
    fn migrations_idempotent_and_versioned() {
        let path = temp_path("migrate");
        {
            let store = Store::open(&path).unwrap();
            assert_eq!(store.user_version().unwrap(), MIGRATIONS.len());
        }
        // Reopen: migrations re-checked, no re-run, same version.
        let store = Store::open(&path).unwrap();
        assert_eq!(store.user_version().unwrap(), MIGRATIONS.len());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn newer_database_refused() {
        let path = temp_path("newer");
        {
            let conn = Connection::open(&path).unwrap();
            conn.pragma_update(None, "user_version", MIGRATIONS.len() + 1)
                .unwrap();
        }
        let err = Store::open(&path).unwrap_err();
        assert!(matches!(err, StoreError::Newer { .. }), "{err}");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn backup_opens_as_valid_store() {
        let path = temp_path("backup");
        let mut backup = path.clone();
        backup.set_extension("bak.db");
        let _ = std::fs::remove_file(&backup);
        {
            let store = Store::open(&path).unwrap();
            let s = session();
            store.insert_session(&s).unwrap();
            let mut r = AuditRecord::new("operator", "session.new", s.id.as_str(), None);
            store.append_audit(&mut r).unwrap();
            store.backup_to(&backup).unwrap();
        }
        let restored = Store::open(&backup).unwrap();
        assert_eq!(restored.user_version().unwrap(), MIGRATIONS.len());
        assert_eq!(restored.verify_audit_chain().unwrap(), 1);
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(&backup);
    }

    #[test]
    fn consume_meta_is_single_shot_and_exact() {
        let store = Store::open_in_memory().unwrap();
        store.meta_set("t", "tok").unwrap();
        // Wrong value never clears the stored one.
        assert!(!store.consume_meta("t", "wrong").unwrap());
        assert_eq!(store.meta_get("t").unwrap().as_deref(), Some("tok"));
        // The right value clears exactly once.
        assert!(store.consume_meta("t", "tok").unwrap());
        assert!(!store.consume_meta("t", "tok").unwrap());
        assert_eq!(store.meta_get("t").unwrap().as_deref(), Some(""));
    }

    #[test]
    fn fresh_store_reaches_the_latest_schema() {
        let store = Store::open_in_memory().unwrap();
        assert_eq!(
            store.user_version().unwrap(),
            MIGRATIONS.len(),
            "a fresh store must apply every migration"
        );
        // Every schema's read surface exists.
        assert!(store.recent_intents(0).unwrap().is_empty());
        assert!(store.list_identities().unwrap().is_empty());
    }

    #[test]
    fn meta_get_set_roundtrip() {
        let store = Store::open_in_memory().unwrap();
        assert_eq!(store.meta_get("budget.day.50").unwrap(), None);
        store.meta_set("budget.day.50", "1.25").unwrap();
        assert_eq!(
            store.meta_get("budget.day.50").unwrap().as_deref(),
            Some("1.25")
        );
        // upsert overwrites
        store.meta_set("budget.day.50", "2.50").unwrap();
        assert_eq!(
            store.meta_get("budget.day.50").unwrap().as_deref(),
            Some("2.50")
        );
    }

    #[test]
    fn schema_v3_migration_from_v2() {
        // Build a v2 database, then reopen: the identities table appears and
        // the data survives.
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("state.db");
        {
            let conn = rusqlite::Connection::open(&db).unwrap();
            conn.pragma_update(None, "journal_mode", "WAL").unwrap();
            conn.execute_batch(include_str!("schema_v1.sql")).unwrap();
            conn.execute_batch(include_str!("schema_v2.sql")).unwrap();
            conn.pragma_update(None, "user_version", 2).unwrap();
        }
        let store = Store::open(&db).unwrap();
        assert_eq!(store.user_version().unwrap(), MIGRATIONS.len());
        assert!(store.link_identity("telegram", "1", "cli").unwrap());
        assert!(store.recent_intents(1).is_ok());
    }

    #[test]
    fn schema_v4_migration_from_v3() {
        // Build a real v3 database, then reopen: the payload column appears
        // and a cron task with payload round-trips.
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("state.db");
        {
            let conn = rusqlite::Connection::open(&db).unwrap();
            conn.pragma_update(None, "journal_mode", "WAL").unwrap();
            conn.execute_batch(include_str!("schema_v1.sql")).unwrap();
            conn.execute_batch(include_str!("schema_v2.sql")).unwrap();
            conn.execute_batch(include_str!("schema_v3.sql")).unwrap();
            conn.pragma_update(None, "user_version", 3).unwrap();
        }
        let store = Store::open(&db).unwrap();
        assert_eq!(store.user_version().unwrap(), MIGRATIONS.len());
        let spec = ka_agent_core::CronSpec {
            text: "daily digest".into(),
            source: "operator".into(),
            recurrence_ms: Some(86_400_000),
            mode: ka_agent_core::CronMode::NoAgent("calendar".into()),
            at_local: None,
            weekdays: Vec::new(),
            month_day: None,
        };
        let mut task = ka_agent_core::Task::new("cron".into(), "cron", false);
        task.scheduled_at_ms = Some(ka_agent_core::now_ms() + 60_000);
        task.payload = Some(spec.to_payload());
        store.insert_task(&task).unwrap();
        let crons = store.cron_tasks().unwrap();
        assert_eq!(crons.len(), 1);
        assert_eq!(
            ka_agent_core::CronSpec::from_payload(crons[0].payload.as_deref().unwrap_or("")),
            Some(spec)
        );
        // Pre-v4 rows (payload NULL) keep loading; the cron is due at +60 s
        // and not before.
        let plain = ka_agent_core::Task::new("⏰ old".into(), "reminder", false);
        store.insert_task(&plain).unwrap();
        let now = ka_agent_core::now_ms();
        assert!(store.due_tasks(now + 30_000).unwrap().is_empty());
        assert_eq!(store.due_tasks(now + 61_000).unwrap().len(), 1);
        assert!(store.due_tasks(now + 61_000).unwrap()[0].payload.is_some());
    }

    #[test]
    fn schema_v5_migration_from_v4() {
        // Build a real v4 database, then reopen: the tasks table accepts
        // 'missed' (the SEED.md §7.2 late-grace state) and a missed task
        // round-trips and stays out of the live/due sets.
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("state.db");
        {
            let conn = rusqlite::Connection::open(&db).unwrap();
            conn.pragma_update(None, "journal_mode", "WAL").unwrap();
            conn.execute_batch(include_str!("schema_v1.sql")).unwrap();
            conn.execute_batch(include_str!("schema_v2.sql")).unwrap();
            conn.execute_batch(include_str!("schema_v3.sql")).unwrap();
            conn.execute_batch(include_str!("schema_v4.sql")).unwrap();
            conn.pragma_update(None, "user_version", 4).unwrap();
        }
        let store = Store::open(&db).unwrap();
        assert_eq!(store.user_version().unwrap(), MIGRATIONS.len());

        let mut task = ka_agent_core::Task::new("water the plants".into(), "reminder", false);
        task.scheduled_at_ms = Some(ka_agent_core::now_ms() - 2 * 3_600_000);
        store.insert_task(&task).unwrap();
        store.set_task_status(&task.id, "missed").unwrap();
        let row = store.get_task(&task.id).unwrap().unwrap();
        assert_eq!(row.status, ka_agent_core::TaskStatus::Missed);
        assert!(row.status.is_terminal());
        let now = ka_agent_core::now_ms();
        assert!(store.due_tasks(now).unwrap().is_empty());
    }

    #[test]
    fn v7_rewrites_single_weekday_cron_payloads() {
        let dir = std::env::temp_dir().join(format!(
            "ka-store-v7-{}-{}",
            std::process::id(),
            ka_agent_core::now_ms()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("state.db");
        {
            let conn = Connection::open(&path).unwrap();
            for sql in &MIGRATIONS[..6] {
                conn.execute_batch(sql).unwrap();
            }
            conn.pragma_update(None, "user_version", 6).unwrap();
            conn.execute(
                "INSERT INTO tasks(id, title, kind, status, scheduled_at_ms, payload, created_ms, updated_ms)
                 VALUES ('task_w', 'gym', 'cron', 'queued', 1, ?1, 1, 1),
                        ('task_d', 'daily', 'cron', 'queued', 1, ?2, 1, 1)",
                params![
                    r#"{"text":"gym","source":"operator","recurrence_ms":86400000,"mode":{"mode":"agent"},"at_local":"18:00","weekday":4}"#,
                    r#"{"text":"daily","source":"operator","recurrence_ms":86400000,"mode":{"mode":"agent"},"at_local":"08:00"}"#,
                ],
            )
            .unwrap();
        }
        let store = Store::open(&path).unwrap();
        let tasks = store.cron_tasks().unwrap();
        let spec = |id: &str| {
            let t = tasks.iter().find(|t| t.id.as_str() == id).unwrap();
            ka_agent_core::CronSpec::from_payload(t.payload.as_deref().unwrap()).unwrap()
        };
        assert_eq!(spec("task_w").weekdays, vec![4]);
        assert_eq!(spec("task_w").day_rule().describe(), "fri");
        assert!(spec("task_d").weekdays.is_empty());
        assert!(
            !tasks
                .iter()
                .any(|t| t.payload.as_deref().unwrap().contains("\"weekday\""))
        );
        let _ = std::fs::remove_dir_all(dir);
    }
}
