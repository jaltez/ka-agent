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

use std::path::Path;

use ka_agent_core::{
    Approval, ApprovalId, ApprovalKind, ApprovalStatus, AuditRecord, MemoryId, OutboxChunk,
    OutboxStatus, QueueId, QueueItem, RunId, Session, SessionId, SessionStatus, Turn, TurnId,
    TurnStatus,
};
use parking_lot::Mutex;
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
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
const MIGRATIONS: &[&str] = &[include_str!("schema_v1.sql")];

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

/// One FTS hit from [`Store::search_episodes`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EpisodeHit {
    pub session: String,
    pub turn: String,
    pub role: String,
    pub snippet: String,
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
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)?;
            }
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

    // ------------------------------------------------------------------
    // sessions
    // ------------------------------------------------------------------

    /// Persist a brand-new session.
    pub fn insert_session(&self, s: &Session) -> Result<()> {
        let conn = self.lock();
        conn.execute(
            "INSERT INTO sessions(id, workspace, strand_id, strand_path, status, created_ms, updated_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                s.id.as_str(),
                s.workspace.to_string_lossy(),
                s.strand_id,
                s.strand_path.as_deref().map(|p| p.to_string_lossy()),
                status_str(&s.status),
                s.created_ms,
                s.updated_ms
            ],
        )?;
        Ok(())
    }

    /// Apply a status change after validating it against the core session
    /// state machine. The current status is re-read inside the write, so a
    /// stale caller cannot smuggle an illegal edge through.
    pub fn update_session_status(&self, id: &SessionId, next: SessionStatus) -> Result<()> {
        let mut conn = self.lock();
        let tx = conn.transaction()?;
        let current: Option<String> = tx
            .query_row(
                "SELECT status FROM sessions WHERE id = ?1",
                params![id.as_str()],
                |r| r.get(0),
            )
            .optional()?;
        let Some(current) = current else {
            return Err(StoreError::NotFound(format!("session {}", id)));
        };
        let Some(current) = parse_session_status(&current) else {
            return Err(StoreError::NotFound(format!("session {}", id)));
        };
        // Same-status writes are idempotent (engines may emit repeated
        // `idle` boundaries).
        if current == next {
            return Ok(());
        }
        if !current.can_transition_to(next) {
            return Err(StoreError::IllegalTransition {
                what: "session",
                from: status_str(&current),
                to: status_str(&next),
            });
        }
        let changed = tx.execute(
            "UPDATE sessions SET status = ?2, updated_ms = ?3 WHERE id = ?1",
            params![id.as_str(), status_str(&next), ka_agent_core::now_ms()],
        )?;
        if changed == 0 {
            return Err(StoreError::NotFound(format!("session {}", id)));
        }
        tx.commit()?;
        Ok(())
    }

    /// Fetch one session by id.
    pub fn get_session(&self, id: &SessionId) -> Result<Session> {
        let conn = self.lock();
        conn.query_row(
            "SELECT id, workspace, strand_id, strand_path, status, created_ms, updated_ms
             FROM sessions WHERE id = ?1",
            params![id.as_str()],
            row_to_session,
        )
        .optional()?
        .ok_or_else(|| StoreError::NotFound(format!("session {}", id)))
    }

    /// Most recently updated sessions first.
    pub fn list_recent_sessions(&self, limit: usize) -> Result<Vec<Session>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT id, workspace, strand_id, strand_path, status, created_ms, updated_ms
             FROM sessions ORDER BY updated_ms DESC LIMIT ?1",
        )?;
        let rows = stmt.query_map(params![limit as i64], row_to_session)?;
        collect_rows(rows)
    }

    /// Bind (or rebind) a session to a Ka strand.
    pub fn set_strand(&self, id: &SessionId, strand_id: &str, strand_path: &Path) -> Result<()> {
        let conn = self.lock();
        let changed = conn.execute(
            "UPDATE sessions SET strand_id = ?2, strand_path = ?3, updated_ms = ?4 WHERE id = ?1",
            params![
                id.as_str(),
                strand_id,
                strand_path.to_string_lossy(),
                ka_agent_core::now_ms()
            ],
        )?;
        if changed == 0 {
            return Err(StoreError::NotFound(format!("session {}", id)));
        }
        Ok(())
    }

    // ------------------------------------------------------------------
    // turns
    // ------------------------------------------------------------------

    /// Persist a brand-new turn (status `queued`, gap G8 mitigation).
    pub fn insert_turn(&self, t: &Turn) -> Result<()> {
        let conn = self.lock();
        conn.execute(
            "INSERT INTO turns(id, session, inbound_text, status, stop, error_class, error_message, cost_usd, tokens, created_ms, updated_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            params![
                t.id.as_str(),
                t.session.as_str(),
                t.inbound_text,
                status_str(&t.status),
                t.stop,
                t.error_class,
                t.error_message,
                t.cost_usd,
                t.tokens as i64,
                t.created_ms,
                t.updated_ms
            ],
        )?;
        Ok(())
    }

    /// Overwrite a turn after validating `in_db_status -> t.status` against
    /// the core turn state machine. Terminal fields (stop, errors, usage)
    /// travel with the struct; the caller keeps the authoritative copy.
    pub fn update_turn(&self, t: &Turn) -> Result<()> {
        let mut conn = self.lock();
        let tx = conn.transaction()?;
        let current: Option<String> = tx
            .query_row(
                "SELECT status FROM turns WHERE id = ?1",
                params![t.id.as_str()],
                |r| r.get(0),
            )
            .optional()?;
        let Some(current) = current else {
            return Err(StoreError::NotFound(format!("turn {}", t.id)));
        };
        let Some(current) = parse_turn_status(&current) else {
            return Err(StoreError::NotFound(format!("turn {}", t.id)));
        };
        if !current.can_transition_to(t.status) {
            return Err(StoreError::IllegalTransition {
                what: "turn",
                from: status_str(&current),
                to: status_str(&t.status),
            });
        }
        tx.execute(
            "UPDATE turns SET status = ?2, stop = ?3, error_class = ?4, error_message = ?5,
                    cost_usd = ?6, tokens = ?7, updated_ms = ?8
             WHERE id = ?1",
            params![
                t.id.as_str(),
                status_str(&t.status),
                t.stop,
                t.error_class,
                t.error_message,
                t.cost_usd,
                t.tokens as i64,
                t.updated_ms
            ],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Restart reconciliation: any turn still `submitted` or `running` when
    /// the daemon comes up is marked `interrupted` (gap G9). `queued` turns
    /// survive — they have not been handed to the engine yet. Returns the
    /// interrupted ids.
    pub fn reconcile_running_turns(&self, now_ms: i64) -> Result<Vec<TurnId>> {
        let mut conn = self.lock();
        let tx = conn.transaction()?;
        let mut stmt = tx.prepare(
            "SELECT id FROM turns WHERE status IN ('submitted','running') ORDER BY created_ms",
        )?;
        let ids = stmt
            .query_map([], |r| r.get::<_, String>(0).map(TurnId))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        drop(stmt);
        tx.execute(
            "UPDATE turns SET status = 'interrupted', updated_ms = ?1
             WHERE status IN ('submitted','running')",
            params![now_ms],
        )?;
        tx.commit()?;
        Ok(ids)
    }

    /// Newest turns for one session (returned oldest-first within the
    /// window).
    pub fn latest_turns_for_session(&self, session: &SessionId, limit: usize) -> Result<Vec<Turn>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT id, session, inbound_text, status, stop, error_class, error_message,
                    cost_usd, tokens, created_ms, updated_ms
             FROM turns WHERE session = ?1 ORDER BY created_ms DESC LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![session.as_str(), limit as i64], row_to_turn)?;
        let mut out = rows.collect::<std::result::Result<Vec<_>, _>>()?;
        out.reverse();
        Ok(out)
    }

    /// One turn by id, if it exists.
    pub fn get_turn(&self, id: &TurnId) -> Result<Option<Turn>> {
        let conn = self.lock();
        let turn = conn
            .query_row(
                "SELECT id, session, inbound_text, status, stop, error_class, error_message,
                        cost_usd, tokens, created_ms, updated_ms
                 FROM turns WHERE id = ?1",
                params![id.as_str()],
                row_to_turn,
            )
            .optional()?;
        Ok(turn)
    }

    /// Sum of `cost_usd` and `tokens` over turns created in
    /// `[day_start_ms, day_end_ms)` — the budget window query.
    pub fn sum_usage(&self, day_start_ms: i64, day_end_ms: i64) -> Result<(f64, u64)> {
        let conn = self.lock();
        let (cost, tokens): (Option<f64>, Option<i64>) = conn.query_row(
            "SELECT SUM(cost_usd), SUM(tokens) FROM turns
             WHERE created_ms >= ?1 AND created_ms < ?2",
            params![day_start_ms, day_end_ms],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        Ok((cost.unwrap_or(0.0), tokens.unwrap_or(0).max(0) as u64))
    }

    // ------------------------------------------------------------------
    // queue
    // ------------------------------------------------------------------

    /// Enqueue a durable inbound message (gap G4).
    pub fn enqueue(&self, item: &QueueItem) -> Result<()> {
        let conn = self.lock();
        conn.execute(
            "INSERT INTO queue(id, session, text, source, enqueued_ms, turned)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                item.id.as_str(),
                item.session.as_str(),
                item.text,
                item.source,
                item.enqueued_ms,
                item.turned.as_ref().map(|t| t.as_str())
            ],
        )?;
        Ok(())
    }

    /// Oldest not-yet-turned queue item for the session (FIFO).
    pub fn next_queued(&self, session: &SessionId) -> Result<Option<QueueItem>> {
        let conn = self.lock();
        conn.query_row(
            "SELECT id, session, text, source, enqueued_ms, turned
             FROM queue WHERE session = ?1 AND turned IS NULL
             ORDER BY enqueued_ms, id LIMIT 1",
            params![session.as_str()],
            |r| {
                Ok(QueueItem {
                    id: QueueId(r.get::<_, String>(0)?),
                    session: SessionId(r.get::<_, String>(1)?),
                    text: r.get(2)?,
                    source: r.get(3)?,
                    enqueued_ms: r.get(4)?,
                    turned: r.get::<_, Option<String>>(5)?.map(TurnId),
                })
            },
        )
        .optional()
        .map_err(StoreError::from)
    }

    /// Record that a queue item became a turn.
    pub fn mark_turned(&self, id: &QueueId, turn: &TurnId) -> Result<()> {
        let conn = self.lock();
        let changed = conn.execute(
            "UPDATE queue SET turned = ?2 WHERE id = ?1",
            params![id.as_str(), turn.as_str()],
        )?;
        if changed == 0 {
            return Err(StoreError::NotFound(format!("queue item {}", id)));
        }
        Ok(())
    }

    /// Number of not-yet-turned queue items for the session.
    pub fn queue_len(&self, session: &SessionId) -> Result<usize> {
        let conn = self.lock();
        let n: i64 = conn.query_row(
            "SELECT COUNT(*) FROM queue WHERE session = ?1 AND turned IS NULL",
            params![session.as_str()],
            |r| r.get(0),
        )?;
        Ok(n as usize)
    }

    // ------------------------------------------------------------------
    // outbox
    // ------------------------------------------------------------------

    /// Insert (or replace, on retry) an outbound chunk keyed by
    /// `(turn, chunk_index)` — a crash mid-send can neither reorder nor
    /// duplicate beyond the chunk in flight.
    pub fn push_chunk(&self, chunk: &OutboxChunk) -> Result<()> {
        let conn = self.lock();
        conn.execute(
            "INSERT OR REPLACE INTO outbox(turn, chunk_index, text, status, channel_message_id, created_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                chunk.turn.as_str(),
                chunk.chunk_index as i64,
                chunk.text,
                status_str(&chunk.status),
                chunk.channel_message_id,
                chunk.created_ms
            ],
        )?;
        Ok(())
    }

    /// Unsent chunks for a turn, in order.
    pub fn pending_chunks(&self, turn: &TurnId) -> Result<Vec<OutboxChunk>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT turn, chunk_index, text, status, channel_message_id, created_ms
             FROM outbox WHERE turn = ?1 AND status = 'queued'
             ORDER BY chunk_index",
        )?;
        let rows = stmt.query_map(params![turn.as_str()], |r| {
            Ok(OutboxChunk {
                turn: TurnId(r.get::<_, String>(0)?),
                chunk_index: r.get::<_, i64>(1)? as u32,
                text: r.get(2)?,
                status: parse_outbox_status(&r.get::<_, String>(3)?)
                    .unwrap_or(OutboxStatus::Failed),
                channel_message_id: r.get(4)?,
                created_ms: r.get(5)?,
            })
        })?;
        collect_rows(rows)
    }

    /// Every chunk for a turn (any status), in order — history rendering.
    pub fn chunks_for_turn(&self, turn: &TurnId) -> Result<Vec<OutboxChunk>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT turn, chunk_index, text, status, channel_message_id, created_ms
             FROM outbox WHERE turn = ?1
             ORDER BY chunk_index",
        )?;
        let rows = stmt.query_map(params![turn.as_str()], |r| {
            Ok(OutboxChunk {
                turn: TurnId(r.get::<_, String>(0)?),
                chunk_index: r.get::<_, i64>(1)? as u32,
                text: r.get(2)?,
                status: parse_outbox_status(&r.get::<_, String>(3)?)
                    .unwrap_or(OutboxStatus::Failed),
                channel_message_id: r.get(4)?,
                created_ms: r.get(5)?,
            })
        })?;
        collect_rows(rows)
    }

    /// Mark a chunk delivered with its channel message id.
    pub fn mark_sent(
        &self,
        turn: &TurnId,
        chunk_index: u32,
        channel_message_id: i64,
    ) -> Result<()> {
        let conn = self.lock();
        let changed = conn.execute(
            "UPDATE outbox SET status = 'sent', channel_message_id = ?3
             WHERE turn = ?1 AND chunk_index = ?2",
            params![turn.as_str(), chunk_index as i64, channel_message_id],
        )?;
        if changed == 0 {
            return Err(StoreError::NotFound(format!(
                "outbox chunk {}/{}",
                turn, chunk_index
            )));
        }
        Ok(())
    }

    // ------------------------------------------------------------------
    // approvals
    // ------------------------------------------------------------------

    /// Persist a new (pending) approval.
    pub fn insert_approval(&self, a: &Approval) -> Result<()> {
        let conn = self.lock();
        conn.execute(
            "INSERT INTO approvals(id, session, turn, kind_json, status, expires_at_ms, decided_at_ms, created_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                a.id.as_str(),
                a.session.as_str(),
                a.turn.as_str(),
                serde_json::to_string(&a.kind).unwrap_or_default(),
                status_str(&a.status),
                a.expires_at_ms,
                a.decided_at_ms,
                a.created_ms
            ],
        )?;
        Ok(())
    }

    /// Pending approvals for a session, oldest first.
    pub fn pending_approvals(&self, session: &SessionId) -> Result<Vec<Approval>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT id, session, turn, kind_json, status, expires_at_ms, decided_at_ms, created_ms
             FROM approvals WHERE session = ?1 AND status = 'pending'
             ORDER BY created_ms",
        )?;
        let rows = stmt.query_map(params![session.as_str()], row_to_approval)?;
        collect_rows(rows)
    }

    /// Fetch one approval by id.
    pub fn get_approval(&self, id: &ApprovalId) -> Result<Approval> {
        let conn = self.lock();
        conn.query_row(
            "SELECT id, session, turn, kind_json, status, expires_at_ms, decided_at_ms, created_ms
             FROM approvals WHERE id = ?1",
            params![id.as_str()],
            row_to_approval,
        )
        .optional()?
        .ok_or_else(|| StoreError::NotFound(format!("approval {}", id)))
    }

    /// Apply a decision after validating it against the core approval state
    /// machine (pending -> approved/denied/expired/cancelled/superseded).
    pub fn decide_approval(&self, id: &ApprovalId, next: ApprovalStatus, at_ms: i64) -> Result<()> {
        let mut conn = self.lock();
        let tx = conn.transaction()?;
        let current: Option<String> = tx
            .query_row(
                "SELECT status FROM approvals WHERE id = ?1",
                params![id.as_str()],
                |r| r.get(0),
            )
            .optional()?;
        let Some(current) = current else {
            return Err(StoreError::NotFound(format!("approval {}", id)));
        };
        let Some(current) = parse_approval_status(&current) else {
            return Err(StoreError::NotFound(format!("approval {}", id)));
        };
        if !current.can_transition_to(next) {
            return Err(StoreError::IllegalTransition {
                what: "approval",
                from: status_str(&current),
                to: status_str(&next),
            });
        }
        tx.execute(
            "UPDATE approvals SET status = ?2, decided_at_ms = ?3 WHERE id = ?1",
            params![id.as_str(), status_str(&next), at_ms],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Expire every pending approval whose TTL has passed; returns the ids.
    pub fn expire_pending(&self, now_ms: i64) -> Result<Vec<ApprovalId>> {
        let mut conn = self.lock();
        let tx = conn.transaction()?;
        let mut stmt = tx
            .prepare("SELECT id FROM approvals WHERE status = 'pending' AND expires_at_ms <= ?1")?;
        let ids = stmt
            .query_map(params![now_ms], |r| r.get::<_, String>(0).map(ApprovalId))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        drop(stmt);
        tx.execute(
            "UPDATE approvals SET status = 'expired', decided_at_ms = ?1
             WHERE status = 'pending' AND expires_at_ms <= ?1",
            params![now_ms],
        )?;
        tx.commit()?;
        Ok(ids)
    }

    // ------------------------------------------------------------------
    // audit
    // ------------------------------------------------------------------

    /// Append one audit record to the hash chain.
    ///
    /// Assigns `seq`, sets `prev_hash` to the previous record's `hash`
    /// (empty string for the first record), and computes
    /// `hash = hex(sha256(prev_hash_utf8 || canonical_json))` where
    /// `canonical_json` is `serde_json`'s serialization of
    /// [`AuditPayload`] — a dedicated struct whose *field declaration order*
    /// (`seq, at_ms, actor, action, subject, detail`) is the canonical
    /// form. `serde_json` emits struct fields in declaration order, so the
    /// bytes are stable across builds; [`Store::verify_audit_chain`]
    /// recomputes them exactly this way.
    pub fn append_audit(&self, record: &mut AuditRecord) -> Result<()> {
        let mut conn = self.lock();
        let tx = conn.transaction()?;
        let prev_hash: Option<String> = tx
            .query_row(
                "SELECT hash FROM audit ORDER BY seq DESC LIMIT 1",
                [],
                |r| r.get(0),
            )
            .optional()?;
        let prev_hash = prev_hash.unwrap_or_default();
        let seq: i64 = tx.query_row("SELECT COALESCE(MAX(seq), 0) + 1 FROM audit", [], |r| {
            r.get(0)
        })?;
        record.seq = seq;
        record.prev_hash = prev_hash.clone();
        let payload = AuditPayload {
            seq,
            at_ms: record.at_ms,
            actor: &record.actor,
            action: &record.action,
            subject: &record.subject,
            detail: record.detail.as_deref(),
        };
        let canonical = serde_json::to_string(&payload).unwrap_or_default();
        let hash = audit_hash(&prev_hash, &canonical);
        tx.execute(
            "INSERT INTO audit(seq, at_ms, actor, action, subject, detail, prev_hash, hash)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                seq,
                record.at_ms,
                record.actor,
                record.action,
                record.subject,
                record.detail,
                prev_hash,
                hash
            ],
        )?;
        record.hash = hash;
        tx.commit()?;
        Ok(())
    }

    /// Recompute every audit row's hash chain; returns the verified record
    /// count or [`StoreError::BrokenAuditChain`] at the first mismatch.
    pub fn verify_audit_chain(&self) -> Result<usize> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT seq, at_ms, actor, action, subject, detail, prev_hash, hash
             FROM audit ORDER BY seq",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, String>(4)?,
                r.get::<_, Option<String>>(5)?,
                r.get::<_, String>(6)?,
                r.get::<_, String>(7)?,
            ))
        })?;
        let mut expected_prev = String::new();
        let mut count = 0usize;
        for row in rows {
            let (seq, at_ms, actor, action, subject, detail, prev_hash, hash) = row?;
            if prev_hash != expected_prev {
                return Err(StoreError::BrokenAuditChain {
                    seq,
                    why: "prev_hash does not link to the previous record",
                });
            }
            let payload = AuditPayload {
                seq,
                at_ms,
                actor: &actor,
                action: &action,
                subject: &subject,
                detail: detail.as_deref(),
            };
            let canonical = serde_json::to_string(&payload).unwrap_or_default();
            if audit_hash(&expected_prev, &canonical) != hash {
                return Err(StoreError::BrokenAuditChain {
                    seq,
                    why: "hash mismatch",
                });
            }
            expected_prev = hash;
            count += 1;
        }
        Ok(count)
    }

    // ------------------------------------------------------------------
    // memory candidates
    // ------------------------------------------------------------------

    /// Record a candidate memory extracted from a conversation.
    pub fn add_candidate(
        &self,
        id: &MemoryId,
        source: &str,
        text: &str,
        provenance: Option<&str>,
    ) -> Result<()> {
        let conn = self.lock();
        conn.execute(
            "INSERT INTO memory_candidates(id, source, text, provenance, status, created_ms)
             VALUES (?1, ?2, ?3, ?4, 'pending', ?5)",
            params![
                id.as_str(),
                source,
                text,
                provenance,
                ka_agent_core::now_ms()
            ],
        )?;
        Ok(())
    }

    /// All candidates still awaiting review.
    pub fn pending_candidates(&self) -> Result<Vec<MemoryId>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT id FROM memory_candidates WHERE status = 'pending' ORDER BY created_ms",
        )?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(0).map(MemoryId))?;
        collect_rows(rows)
    }

    /// Tasks of a kind whose scheduled time has passed (scheduler).
    pub fn due_tasks(&self, now_ms: i64, kind: &str) -> Result<Vec<ka_agent_core::Task>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT id, session, title, kind, status, read_only, scheduled_at_ms, created_ms, updated_ms
             FROM tasks
             WHERE kind = ?1 AND status = 'queued' AND scheduled_at_ms IS NOT NULL AND scheduled_at_ms <= ?2
             ORDER BY scheduled_at_ms",
        )?;
        let rows = stmt.query_map(params![kind, now_ms], row_to_task)?;
        collect_rows(rows)
    }

    /// Insert a task row.
    pub fn insert_task(&self, t: &ka_agent_core::Task) -> Result<()> {
        let conn = self.lock();
        conn.execute(
            "INSERT INTO tasks(id, session, title, kind, status, read_only, scheduled_at_ms, created_ms, updated_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                t.id.as_str(),
                t.session.as_ref().map(|s| s.as_str()),
                t.title,
                t.kind,
                task_status_str(&t.status),
                t.read_only as i64,
                t.scheduled_at_ms,
                t.created_ms,
                t.updated_ms
            ],
        )?;
        Ok(())
    }

    /// Update a task's status.
    pub fn set_task_status(&self, id: &ka_agent_core::TaskId, status: &str) -> Result<()> {
        let conn = self.lock();
        conn.execute(
            "UPDATE tasks SET status = ?2, updated_ms = ?3 WHERE id = ?1",
            params![id.as_str(), status, ka_agent_core::now_ms()],
        )?;
        Ok(())
    }

    /// Pending candidates with full text, for `/memory review`.
    pub fn pending_candidates_full(&self) -> Result<Vec<CandidateRow>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT id, source, text, provenance, created_ms
             FROM memory_candidates WHERE status = 'pending' ORDER BY created_ms",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(CandidateRow {
                id: MemoryId(r.get::<_, String>(0)?),
                source: r.get(1)?,
                text: r.get(2)?,
                provenance: r.get(3)?,
                created_ms: r.get(4)?,
            })
        })?;
        collect_rows(rows)
    }

    /// Set a candidate's review outcome (`accepted`/`rejected`/`discarded`;
    /// anything else is rejected by the schema CHECK).
    pub fn set_candidate_status(&self, id: &MemoryId, status: &str) -> Result<()> {
        let conn = self.lock();
        let changed = conn.execute(
            "UPDATE memory_candidates SET status = ?2 WHERE id = ?1",
            params![id.as_str(), status],
        )?;
        if changed == 0 {
            return Err(StoreError::NotFound(format!("candidate {}", id)));
        }
        Ok(())
    }

    // ------------------------------------------------------------------
    // runs
    // ------------------------------------------------------------------

    /// Record the start of a background run.
    pub fn insert_run(&self, run: &RunRow) -> Result<()> {
        let conn = self.lock();
        conn.execute(
            "INSERT INTO runs(id, kind, ref_id, status, stop, error_class, error_message, cost_usd, tokens, started_ms, ended_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            params![
                run.id.as_str(),
                run.kind,
                run.ref_id,
                run.status,
                run.stop,
                run.error_class,
                run.error_message,
                run.cost_usd,
                run.tokens.map(|t| t as i64),
                run.started_ms,
                run.ended_ms
            ],
        )?;
        Ok(())
    }

    /// Fill in a run's terminal fields.
    #[allow(clippy::too_many_arguments)]
    pub fn finish_run(
        &self,
        id: &RunId,
        status: &str,
        stop: Option<&str>,
        error_class: Option<&str>,
        error_message: Option<&str>,
        cost_usd: Option<f64>,
        tokens: Option<u64>,
        ended_ms: i64,
    ) -> Result<()> {
        let conn = self.lock();
        let changed = conn.execute(
            "UPDATE runs SET status = ?2, stop = ?3, error_class = ?4, error_message = ?5,
                    cost_usd = ?6, tokens = ?7, ended_ms = ?8
             WHERE id = ?1",
            params![
                id.as_str(),
                status,
                stop,
                error_class,
                error_message,
                cost_usd,
                tokens.map(|t| t as i64),
                ended_ms
            ],
        )?;
        if changed == 0 {
            return Err(StoreError::NotFound(format!("run {}", id)));
        }
        Ok(())
    }

    /// Most recent runs, newest first.
    pub fn recent_runs(&self, limit: usize) -> Result<Vec<RunRow>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT id, kind, ref_id, status, stop, error_class, error_message, cost_usd, tokens, started_ms, ended_ms
             FROM runs ORDER BY started_ms DESC LIMIT ?1",
        )?;
        let rows = stmt.query_map(params![limit as i64], |r| {
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
        })?;
        collect_rows(rows)
    }

    // ------------------------------------------------------------------
    // episodes (FTS5)
    // ------------------------------------------------------------------

    /// Index one episode message for search.
    pub fn index_episode(
        &self,
        session: &SessionId,
        turn: &TurnId,
        role: &str,
        content: &str,
    ) -> Result<()> {
        let conn = self.lock();
        conn.execute(
            "INSERT INTO episodes_fts(session, turn, role, content) VALUES (?1, ?2, ?3, ?4)",
            params![session.as_str(), turn.as_str(), role, content],
        )?;
        Ok(())
    }

    /// Phrase-quoted FTS search (like ka-index: the raw query is wrapped in
    /// double quotes with inner quotes doubled, so it is never interpreted
    /// as FTS syntax). Returns hits with a snippet around the match.
    pub fn search_episodes(&self, query: &str, limit: usize) -> Result<Vec<EpisodeHit>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT session, turn, role, snippet(episodes_fts, 3, '[', ']', '…', 12)
             FROM episodes_fts WHERE episodes_fts MATCH ?1
             ORDER BY rank LIMIT ?2",
        )?;
        let quoted = format!("\"{}\"", query.replace('"', "\"\""));
        let rows = stmt.query_map(params![quoted, limit as i64], |r| {
            Ok(EpisodeHit {
                session: r.get(0)?,
                turn: r.get(1)?,
                role: r.get(2)?,
                snippet: r.get(3)?,
            })
        })?;
        collect_rows(rows)
    }

    /// Remove a session's episodes (strand reset / purge).
    pub fn delete_session_episodes(&self, session: &SessionId) -> Result<()> {
        let conn = self.lock();
        conn.execute(
            "DELETE FROM episodes_fts WHERE session = ?1",
            params![session.as_str()],
        )?;
        Ok(())
    }
}

/// Canonical audit serialization target. Field order below IS the canonical
/// form; see [`Store::append_audit`].
#[derive(Serialize)]
struct AuditPayload<'a> {
    seq: i64,
    at_ms: i64,
    actor: &'a str,
    action: &'a str,
    subject: &'a str,
    detail: Option<&'a str>,
}

fn audit_hash(prev_hash: &str, canonical_json: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(prev_hash.as_bytes());
    hasher.update(canonical_json.as_bytes());
    hex(&hasher.finalize())
}

fn hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0xf) as usize] as char);
    }
    out
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

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use ka_agent_core::ApprovalKind;
    use std::path::PathBuf;

    fn temp_path(tag: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("ka-agent-store-{}-{}.db", tag, std::process::id()));
        let _ = std::fs::remove_file(&p);
        p
    }

    fn session() -> Session {
        Session::new(PathBuf::from("/tmp/ws"))
    }

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
    fn session_status_legality_enforced() {
        let store = Store::open_in_memory().unwrap();
        let s = session();
        store.insert_session(&s).unwrap();
        store
            .update_session_status(&s.id, SessionStatus::Running)
            .unwrap();
        store
            .update_session_status(&s.id, SessionStatus::Stopping)
            .unwrap();
        // stopping -> running is illegal
        let err = store
            .update_session_status(&s.id, SessionStatus::Running)
            .unwrap_err();
        assert!(matches!(err, StoreError::IllegalTransition { .. }), "{err}");
        // unknown id
        let err = store
            .update_session_status(&SessionId::generate(), SessionStatus::Idle)
            .unwrap_err();
        assert!(matches!(err, StoreError::NotFound { .. }), "{err}");
        // set_strand + get + list
        store
            .set_strand(&s.id, "str-1", Path::new("/tmp/str.jsonl"))
            .unwrap();
        let got = store.get_session(&s.id).unwrap();
        assert_eq!(got.strand_id.as_deref(), Some("str-1"));
        assert_eq!(store.list_recent_sessions(10).unwrap().len(), 1);
    }

    #[test]
    fn turn_reconcile_marks_only_in_flight() {
        let store = Store::open_in_memory().unwrap();
        let s = session();
        store.insert_session(&s).unwrap();
        let mut queued = Turn::new(s.id.clone(), "q".into());
        let mut submitted = Turn::new(s.id.clone(), "s".into());
        let mut running = Turn::new(s.id.clone(), "r".into());
        submitted.status = TurnStatus::Submitted;
        running.status = TurnStatus::Running;
        store.insert_turn(&queued).unwrap();
        store.insert_turn(&submitted).unwrap();
        store.insert_turn(&running).unwrap();

        let ids = store.reconcile_running_turns(1234).unwrap();
        assert_eq!(ids.len(), 2);
        let after = store.latest_turns_for_session(&s.id, 10).unwrap();
        let q = after.iter().find(|t| t.inbound_text == "q").unwrap();
        let sub = after.iter().find(|t| t.inbound_text == "s").unwrap();
        let run = after.iter().find(|t| t.inbound_text == "r").unwrap();
        assert_eq!(q.status, TurnStatus::Queued);
        assert_eq!(sub.status, TurnStatus::Interrupted);
        assert_eq!(run.status, TurnStatus::Interrupted);

        // update_turn legality against the in-DB status
        let mut done = run.clone();
        done.status = TurnStatus::Done; // interrupted -> done is illegal
        let err = store.update_turn(&done).unwrap_err();
        assert!(matches!(err, StoreError::IllegalTransition { .. }), "{err}");
        // queued -> submitted via update_turn is legal
        queued.status = TurnStatus::Submitted;
        store.update_turn(&queued).unwrap();
    }

    #[test]
    fn queue_fifo_and_mark_turned() {
        let store = Store::open_in_memory().unwrap();
        let s = session();
        store.insert_session(&s).unwrap();
        let a = QueueItem::new(s.id.clone(), "first".into(), "telegram:1");
        let mut b = QueueItem::new(s.id.clone(), "second".into(), "telegram:1");
        b.enqueued_ms += 10; // deterministic order regardless of clock granularity
        store.enqueue(&a).unwrap();
        store.enqueue(&b).unwrap();
        assert_eq!(store.queue_len(&s.id).unwrap(), 2);
        let next = store.next_queued(&s.id).unwrap().unwrap();
        assert_eq!(next.text, "first");
        let t = TurnId::generate();
        store.mark_turned(&next.id, &t).unwrap();
        assert_eq!(store.queue_len(&s.id).unwrap(), 1);
        let next = store.next_queued(&s.id).unwrap().unwrap();
        assert_eq!(next.text, "second");
        assert_eq!(next.turned, None);
    }

    #[test]
    fn outbox_ordering_and_mark_sent() {
        let store = Store::open_in_memory().unwrap();
        let turn = TurnId::generate();
        store
            .push_chunk(&OutboxChunk::new(turn.clone(), 1, "b".into()))
            .unwrap();
        store
            .push_chunk(&OutboxChunk::new(turn.clone(), 0, "a".into()))
            .unwrap();
        let pending = store.pending_chunks(&turn).unwrap();
        assert_eq!(pending.len(), 2);
        assert_eq!(pending[0].text, "a");
        assert_eq!(pending[1].text, "b");
        store.mark_sent(&turn, 0, 4242).unwrap();
        let pending = store.pending_chunks(&turn).unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].text, "b");
        let err = store.mark_sent(&turn, 9, 1).unwrap_err();
        assert!(matches!(err, StoreError::NotFound { .. }), "{err}");
    }

    #[test]
    fn approvals_decide_and_expire() {
        let store = Store::open_in_memory().unwrap();
        let s = session();
        store.insert_session(&s).unwrap();
        let turn = TurnId::generate();
        let a = Approval::new(
            s.id.clone(),
            turn.clone(),
            ApprovalKind::KaAsk {
                ask_id: "ask-1".into(),
                question: "q?".into(),
                detail: None,
                options: vec!["allow".into()],
            },
            1000,
        );
        store.insert_approval(&a).unwrap();
        let b = Approval::new(
            s.id.clone(),
            turn,
            ApprovalKind::Intent {
                tool: "shell".into(),
                args_summary: "rm".into(),
                effect: "deletes".into(),
            },
            -1, // already past expiry
        );
        store.insert_approval(&b).unwrap();
        assert_eq!(store.pending_approvals(&s.id).unwrap().len(), 2);

        store
            .decide_approval(&a.id, ApprovalStatus::Approved, 55)
            .unwrap();
        let got = store.get_approval(&a.id).unwrap();
        assert_eq!(got.status, ApprovalStatus::Approved);
        assert_eq!(got.decided_at_ms, Some(55));
        // terminal -> anything is illegal
        let err = store
            .decide_approval(&a.id, ApprovalStatus::Denied, 56)
            .unwrap_err();
        assert!(matches!(err, StoreError::IllegalTransition { .. }), "{err}");

        let expired = store.expire_pending(ka_agent_core::now_ms()).unwrap();
        assert_eq!(expired, vec![b.id.clone()]);
        assert_eq!(
            store.get_approval(&b.id).unwrap().status,
            ApprovalStatus::Expired
        );
        assert_eq!(store.pending_approvals(&s.id).unwrap().len(), 0);
    }

    #[test]
    fn audit_chain_append_verify_tamper() {
        let store = Store::open_in_memory().unwrap();
        for i in 0..3 {
            let mut r = AuditRecord::new("operator", "test.event", &format!("s{i}"), None);
            store.append_audit(&mut r).unwrap();
            assert_eq!(r.seq, i + 1);
            assert_eq!(r.hash.len(), 64);
        }
        assert_eq!(store.verify_audit_chain().unwrap(), 3);

        {
            let conn = store.lock();
            conn.execute("UPDATE audit SET actor = 'mallory' WHERE seq = 2", [])
                .unwrap();
        }
        let err = store.verify_audit_chain().unwrap_err();
        assert!(
            matches!(err, StoreError::BrokenAuditChain { seq: 2, .. }),
            "{err}"
        );
    }

    #[test]
    fn episodes_index_search_delete() {
        let store = Store::open_in_memory().unwrap();
        let s1 = session();
        let s2 = Session::new(PathBuf::from("/tmp/ws2"));
        store.insert_session(&s1).unwrap();
        store.insert_session(&s2).unwrap();
        let t = TurnId::generate();
        store
            .index_episode(&s1.id, &t, "user", "fix the flaky mutex test")
            .unwrap();
        store
            .index_episode(&s1.id, &t, "assistant", "the mutex was double-locked")
            .unwrap();
        store
            .index_episode(&s2.id, &t, "user", "unrelated grocery list")
            .unwrap();

        let hits = store.search_episodes("mutex", 10).unwrap();
        assert_eq!(hits.len(), 2);
        assert!(hits.iter().all(|h| h.session == *s1.id.as_str()));

        store.delete_session_episodes(&s1.id).unwrap();
        assert_eq!(store.search_episodes("mutex", 10).unwrap().len(), 0);
        assert_eq!(store.search_episodes("grocery", 10).unwrap().len(), 1);
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
    fn sum_usage_window_math() {
        let store = Store::open_in_memory().unwrap();
        let s = session();
        store.insert_session(&s).unwrap();
        let day0 = 86_400_000i64 * 50; // arbitrary day start
        for (i, cost) in [(0i64, 1.5f64), (1, 2.5), (2, 10.0)] {
            let mut t = Turn::new(s.id.clone(), format!("t{i}"));
            t.created_ms = day0 + i * 1000;
            t.cost_usd = cost;
            t.tokens = (i as u64 + 1) * 100;
            store.insert_turn(&t).unwrap();
        }
        // window covers the first two turns only
        let (cost, tokens) = store.sum_usage(day0, day0 + 1500).unwrap();
        assert!((cost - 4.0).abs() < 1e-9, "{cost}");
        assert_eq!(tokens, 300);
        // full window
        let (cost, tokens) = store.sum_usage(day0, day0 + 86_400_000).unwrap();
        assert!((cost - 14.0).abs() < 1e-9, "{cost}");
        assert_eq!(tokens, 600);
        // empty window
        assert_eq!(store.sum_usage(0, 1).unwrap(), (0.0, 0));
    }

    #[test]
    fn runs_and_candidates() {
        let store = Store::open_in_memory().unwrap();
        let run = RunRow {
            id: RunId::generate(),
            kind: "task".into(),
            ref_id: Some("task-1".into()),
            status: "running".into(),
            stop: None,
            error_class: None,
            error_message: None,
            cost_usd: None,
            tokens: None,
            started_ms: 100,
            ended_ms: None,
        };
        store.insert_run(&run).unwrap();
        store
            .finish_run(
                &run.id,
                "succeeded",
                Some("done"),
                None,
                None,
                Some(0.25),
                Some(50),
                200,
            )
            .unwrap();
        let runs = store.recent_runs(5).unwrap();
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].status, "succeeded");
        assert_eq!(runs[0].tokens, Some(50));

        let mem = MemoryId::generate();
        store
            .add_candidate(&mem, "session", "prefers vim", Some("turn-9"))
            .unwrap();
        assert_eq!(store.pending_candidates().unwrap(), vec![mem.clone()]);
        store.set_candidate_status(&mem, "accepted").unwrap();
        assert!(store.pending_candidates().unwrap().is_empty());
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
        created_ms: r.get(7)?,
        updated_ms: r.get(8)?,
    })
}
