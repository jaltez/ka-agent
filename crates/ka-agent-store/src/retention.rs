//! Retention (plan 1.8): bounded state. Terminal runs, settled approvals,
//! finished intents, decided memory candidates, turned queue rows, sent
//! outbox chunks and terminal reminders are pruned on a schedule; turns,
//! episodes_fts, sessions, audit (append-only by policy), identities and
//! crons are never pruned.

use super::{Store, StoreError};

/// What one prune pass deleted, per table.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PruneReport {
    pub runs: usize,
    /// Turns deleted by `retention.turns_days` (0 = never).
    pub turns: usize,
    pub approvals: usize,
    pub intents: usize,
    pub memory_candidates: usize,
    pub queue: usize,
    pub outbox: usize,
    pub tasks: usize,
    /// Checkpoint rows deleted by the checkpoint window (B1; the shadow
    /// repos follow via `checkpoint_dir_keys` at the scheduler).
    pub checkpoints: usize,
}

const RUNS_DAYS: i64 = 90;
const APPROVALS_DAYS: i64 = 90;
const INTENTS_DAYS: i64 = 90;
const CANDIDATES_DAYS: i64 = 90;
const TASKS_DAYS: i64 = 90;
const QUEUE_DAYS: i64 = 30;
const OUTBOX_DAYS: i64 = 30;
/// Gemini's documented default; `checkpoint.keep_days` takes over when
/// its config key lands (core-owned `config_keys.rs`).
const CHECKPOINTS_DAYS: u32 = 30;

impl Store {
    /// Delete terminal rows older than the retention windows. One
    /// transaction; safe to run at any time. Turns are only touched when
    /// `turns_days > 0` (the RGPD knob, default keep-everything).
    pub fn prune(&self, now_ms: i64) -> Result<PruneReport, StoreError> {
        self.prune_with(now_ms, 0)
    }

    /// The same, with an explicit turns window in days (0 = never).
    pub fn prune_with(&self, now_ms: i64, turns_days: u32) -> Result<PruneReport, StoreError> {
        self.prune_all(now_ms, turns_days, CHECKPOINTS_DAYS)
    }

    /// The full pass with the checkpoint window from
    /// `[checkpoint] keep_days` (B1); the plain [`prune_with`](Self::prune_with)
    /// keeps the documented default for every existing caller.
    pub fn prune_with_checkpoints(
        &self,
        now_ms: i64,
        turns_days: u32,
        checkpoints_days: u32,
    ) -> Result<PruneReport, StoreError> {
        self.prune_all(now_ms, turns_days, checkpoints_days)
    }

    fn prune_all(
        &self,
        now_ms: i64,
        turns_days: u32,
        checkpoints_days: u32,
    ) -> Result<PruneReport, StoreError> {
        let day = 86_400_000i64;
        let mut conn = self.lock();
        let tx = conn.transaction()?;
        let exec = |tx: &rusqlite::Transaction<'_>,
                    sql: &str,
                    cutoff: i64|
         -> Result<usize, StoreError> { Ok(tx.execute(sql, [cutoff])?) };
        let turns = if turns_days == 0 {
            0
        } else {
            // Their search rows go with them, or the FTS index would keep
            // text the retention window promised to delete.
            let cutoff = now_ms - (turns_days as i64) * day;
            tx.execute(
                "DELETE FROM episodes_fts WHERE turn IN \
                 (SELECT id FROM turns WHERE status IN ('done','error','aborted','interrupted','killed') AND created_ms < ?1)",
                [cutoff],
            )?;
            tx.execute(
                "DELETE FROM turn_sources WHERE turn_id IN \
                 (SELECT id FROM turns WHERE status IN ('done','error','aborted','interrupted','killed') AND created_ms < ?1)",
                [cutoff],
            )?;
            tx.execute(
                "DELETE FROM turn_origins WHERE turn_id IN \
                 (SELECT id FROM turns WHERE status IN ('done','error','aborted','interrupted','killed') AND created_ms < ?1)",
                [cutoff],
            )?;
            tx.execute(
                "DELETE FROM turns WHERE status IN ('done','error','aborted','interrupted','killed') AND created_ms < ?1",
                [cutoff],
            )?
        };
        let report = PruneReport {
            turns,
            runs: exec(
                &tx,
                "DELETE FROM runs WHERE ended_ms IS NOT NULL AND ended_ms < ?",
                now_ms - RUNS_DAYS * day,
            )?,
            approvals: exec(
                &tx,
                "DELETE FROM approvals WHERE status != 'pending' \
                 AND COALESCE(decided_at_ms, expires_at_ms) < ?",
                now_ms - APPROVALS_DAYS * day,
            )?,
            intents: exec(
                &tx,
                "DELETE FROM intents WHERE status IN ('done','failed','denied') AND updated_ms < ?",
                now_ms - INTENTS_DAYS * day,
            )?,
            memory_candidates: exec(
                &tx,
                "DELETE FROM memory_candidates WHERE status != 'pending' AND created_ms < ?",
                now_ms - CANDIDATES_DAYS * day,
            )?,
            queue: exec(
                &tx,
                "DELETE FROM queue WHERE turned IS NOT NULL AND enqueued_ms < ?",
                now_ms - QUEUE_DAYS * day,
            )?,
            outbox: exec(
                &tx,
                "DELETE FROM outbox WHERE status = 'sent' AND created_ms < ?",
                now_ms - OUTBOX_DAYS * day,
            )?,
            tasks: exec(
                &tx,
                "DELETE FROM tasks WHERE kind = 'reminder' \
                 AND status IN ('succeeded','failed','cancelled','missed') AND updated_ms < ?",
                now_ms - TASKS_DAYS * day,
            )?,
            checkpoints: exec(
                &tx,
                "DELETE FROM checkpoints WHERE created_ms < ?",
                now_ms - (checkpoints_days as i64) * day,
            )?,
        };
        tx.commit()?;
        Ok(report)
    }

    /// The sessions the store still references, as ids: every session
    /// with a turn row (retained by `retention.turns_days`, too fresh to
    /// prune, or still open) and every session with a message still
    /// queued. A strand sweep keeps the strands of exactly these —
    /// `@turn:` and `@session:` read them, and a resumed session needs
    /// its context — plus the live ones only the caller can know.
    pub fn referenced_sessions(&self) -> Result<std::collections::HashSet<String>, StoreError> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT DISTINCT session FROM turns
             UNION SELECT DISTINCT session FROM queue WHERE turned IS NULL",
        )?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
        let mut out = std::collections::HashSet::new();
        for row in rows {
            out.insert(row?);
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use crate::Store;

    #[test]
    fn prune_removes_only_old_terminal_rows() {
        let store = Store::open_in_memory().unwrap();
        let now = 1_800_000_000_000i64; // 2027-01-15TZ
        let day = 86_400_000i64;

        // seed: one old + one fresh row per table, plus rows that must stay
        let old = now - 120 * day;
        let fresh = now - day;
        store.insert_session(&crate::session()).unwrap();
        let s = store
            .list_recent_sessions(1, true)
            .unwrap()
            .pop()
            .map(|sess| sess.id.to_string())
            .unwrap_or_else(|| "s1".to_string());
        store
            .insert_run(&crate::RunRow {
                id: ka_agent_core::RunId("r-old".into()),
                kind: "cron".into(),
                ref_id: Some("t1".into()),
                status: "succeeded".into(),
                stop: Some("done".into()),
                error_class: None,
                error_message: None,
                cost_usd: Some(0.0),
                tokens: Some(1),
                started_ms: old,
                ended_ms: Some(old),
            })
            .unwrap();
        store
            .insert_run(&crate::RunRow {
                id: ka_agent_core::RunId("r-run".into()),
                kind: "cron".into(),
                ref_id: Some("t2".into()),
                status: "running".into(),
                stop: None,
                error_class: None,
                error_message: None,
                cost_usd: None,
                tokens: None,
                started_ms: old,
                ended_ms: None,
            })
            .unwrap();
        // approvals: old decided stays-out, old pending stays-in
        for (id, status, decided) in [
            ("a-old-decided", "denied", Some(old)),
            ("a-old-pending", "pending", None),
            ("a-new-decided", "approved", Some(fresh)),
        ] {
            store
                .lock()
                .execute(
                    "INSERT INTO approvals(id, session, turn, kind_json, status, expires_at_ms, decided_at_ms, created_ms)
                     VALUES (?1, ?2, 'turn', '{}', ?3, ?4, ?5, ?6)",
                    rusqlite::params![
                        id,
                        s,
                        status,
                        if decided.is_some() { old } else { now + 1000 },
                        decided,
                        old
                    ],
                )
                .unwrap();
        }
        // intents
        for (id, status, updated) in [
            ("i-old-done", "done", old),
            ("i-old-proposed", "proposed", old),
            ("i-new-done", "done", fresh),
        ] {
            store
                .lock()
                .execute(
                    "INSERT INTO intents(id, tool, args_hash, status, created_ms, updated_ms)
                     VALUES (?1, 'github', 'h', ?2, ?3, ?4)",
                    rusqlite::params![id, status, old, updated],
                )
                .unwrap();
        }
        // memory candidates: decided-old (pruned) vs pending-old (kept)
        for (id, status) in [("c-old-acc", "accepted"), ("c-old-pend", "pending")] {
            store
                .lock()
                .execute(
                    "INSERT INTO memory_candidates(id, source, text, provenance, status, created_ms)
                     VALUES (?1, 'import', 'text', NULL, ?2, ?3)",
                    rusqlite::params![id, status, old],
                )
                .unwrap();
        }
        // queue: one turned-old, one fresh-unturned
        for (id, turned, enq) in [("q-old", Some("turn-1"), old), ("q-new", None, fresh)] {
            store
                .lock()
                .execute(
                    "INSERT INTO queue(id, session, text, source, enqueued_ms, turned)
                     VALUES (?1, ?2, 'x', 'console', ?3, ?4)",
                    rusqlite::params![id, s, enq, turned],
                )
                .unwrap();
        }
        // outbox: sent-old vs queued-old
        for (idx, status) in [(0, "sent"), (1, "queued")] {
            store
                .lock()
                .execute(
                    "INSERT INTO outbox(turn, chunk_index, text, status, created_ms)
                     VALUES ('turn', ?1, 'x', ?2, ?3)",
                    rusqlite::params![idx, status, old],
                )
                .unwrap();
        }
        // reminder tasks: terminal-old vs pending-old
        for (id, status) in [("task-old", "succeeded"), ("task-pend", "queued")] {
            store
                .lock()
                .execute(
                    "INSERT INTO tasks(id, session, title, kind, status, read_only, scheduled_at_ms, created_ms, updated_ms)
                     VALUES (?1, NULL, 'r', 'reminder', ?2, 0, ?3, ?3, ?3)",
                    rusqlite::params![id, status, old],
                )
                .unwrap();
        }

        // checkpoints: one past the 30-day window, one fresh
        for (id, created) in [("ckpt-old", old), ("ckpt-new", fresh)] {
            store
                .lock()
                .execute(
                    "INSERT INTO checkpoints(id, session, turn, username, sha, created_ms)
                     VALUES (?1, ?2, 'turn', 'operator', 'sha', ?3)",
                    rusqlite::params![id, s, created],
                )
                .unwrap();
        }

        let report = store.prune(now).unwrap();
        assert_eq!(report.runs, 1);
        assert_eq!(report.approvals, 1, "{report:?}");
        assert_eq!(report.intents, 1);
        assert_eq!(report.memory_candidates, 1);
        assert_eq!(report.queue, 1);
        assert_eq!(report.outbox, 1);
        assert_eq!(report.tasks, 1, "{report:?}");
        assert_eq!(report.checkpoints, 1, "{report:?}");

        let count = |table: &str, cond: &str| -> i64 {
            store
                .lock()
                .query_row(
                    &format!("SELECT COUNT(*) FROM {table} WHERE {cond}"),
                    [],
                    |r| r.get(0),
                )
                .unwrap()
        };
        assert_eq!(count("runs", "id = 'r-run'"), 1, "running run stays");
        assert_eq!(count("approvals", "id = 'a-old-pending'"), 1);
        assert_eq!(count("approvals", "id = 'a-new-decided'"), 1);
        assert_eq!(count("intents", "id = 'i-old-proposed'"), 1);
        assert_eq!(count("memory_candidates", "id = 'c-old-pend'"), 1);
        assert_eq!(count("queue", "id = 'q-new'"), 1);
        assert_eq!(count("outbox", "status = 'queued'"), 1);
        assert_eq!(count("tasks", "id = 'task-pend'"), 1);
        // never pruned: turns, sessions, audit, identities, crons
        assert_eq!(count("sessions", &format!("id = '{s}'")), 1);
        assert_eq!(count("checkpoints", "id = 'ckpt-new'"), 1);
    }

    /// A strand sweep keeps the strands of exactly these sessions: one
    /// with a turn row, one with a message still queued — never one whose
    /// every turn was pruned.
    #[test]
    fn referenced_sessions_own_a_turn_or_a_queued_message() {
        let store = Store::open_in_memory().unwrap();
        let now = 1_800_000_000_000i64;
        let with_turn = crate::session();
        let queued = crate::session();
        let bare = crate::session();
        for s in [&with_turn, &queued, &bare] {
            store.insert_session(s).unwrap();
        }
        let mut turn = ka_agent_core::Turn::new(with_turn.id.clone(), "hi".into());
        turn.status = ka_agent_core::TurnStatus::Done;
        turn.created_ms = now - 2 * 86_400_000;
        store.insert_turn(&turn).unwrap();
        store
            .enqueue(&ka_agent_core::QueueItem::new(
                queued.id.clone(),
                "later".into(),
                "console",
            ))
            .unwrap();
        let referenced = store.referenced_sessions().unwrap();
        assert!(referenced.contains(with_turn.id.as_str()), "{referenced:?}");
        assert!(referenced.contains(queued.id.as_str()), "{referenced:?}");
        assert!(!referenced.contains(bare.id.as_str()), "{referenced:?}");

        // Its turns pruned, the session is no longer referenced.
        assert_eq!(store.prune_with(now, 1).unwrap().turns, 1);
        let referenced = store.referenced_sessions().unwrap();
        assert!(
            !referenced.contains(with_turn.id.as_str()),
            "{referenced:?}"
        );
        assert!(referenced.contains(queued.id.as_str()));
    }
}
