//! Session persistence: insert, status transitions, lookup, strand binding.
//! Split from lib.rs (moves only).

use super::*;

impl Store {
    /// Persist a brand-new session.
    pub fn insert_session(&self, s: &Session) -> Result<()> {
        let conn = self.lock();
        conn.execute(
            "INSERT INTO sessions(id, workspace, strand_id, strand_path, status, private, created_ms, updated_ms, role)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                s.id.as_str(),
                s.workspace.to_string_lossy(),
                s.strand_id,
                s.strand_path.as_deref().map(|p| p.to_string_lossy()),
                status_str(&s.status),
                s.private as i64,
                s.created_ms,
                s.updated_ms,
                s.role
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
            "SELECT id, workspace, strand_id, strand_path, status, created_ms, updated_ms, private, role
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
            "SELECT id, workspace, strand_id, strand_path, status, created_ms, updated_ms, private, role
             FROM sessions ORDER BY updated_ms DESC LIMIT ?1",
        )?;
        let rows = stmt.query_map(params![limit as i64], row_to_session)?;
        collect_rows(rows)
    }

    /// The most recently updated session of one role (`/resume` after a
    /// role switch; `admin` included).
    pub fn latest_session_for_role(&self, role: &str) -> Result<Option<Session>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT id, workspace, strand_id, strand_path, status, created_ms, updated_ms, private, role
             FROM sessions WHERE role = ?1 ORDER BY updated_ms DESC LIMIT 1",
        )?;
        let rows = stmt.query_map(params![role], row_to_session)?;
        collect_rows(rows).map(|mut v| v.pop())
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

    /// Every private (incognito) session on record, after deleting the
    /// rows that still carry private conversation content: queued
    /// messages (their delivery text) and approval cards (whose
    /// question/detail embed the private turn's tool arguments).
    /// Callers decide which sessions' remnants to destroy: a private
    /// conversation that resumes across a daemon restart keeps its
    /// context until it is superseded. The trailing checkpoint truncates
    /// the WAL so the deleted text leaves the journal too
    /// (`secure_delete` zeroes the freed pages).
    pub fn private_sessions_purge_queue(&self) -> Result<Vec<Session>> {
        let conn = self.lock();
        conn.execute(
            "DELETE FROM queue WHERE session IN (SELECT id FROM sessions WHERE private = 1)",
            [],
        )?;
        conn.execute(
            "DELETE FROM approvals WHERE session IN (SELECT id FROM sessions WHERE private = 1)",
            [],
        )?;
        // Best-effort: a concurrent reader can hold the checkpoint busy.
        let _ = conn.execute("PRAGMA wal_checkpoint(TRUNCATE)", []);
        let mut stmt = conn.prepare(
            "SELECT id, workspace, strand_id, strand_path, status, created_ms, updated_ms, private, role
             FROM sessions WHERE private = 1",
        )?;
        let rows = stmt.query_map([], row_to_session)?;
        collect_rows(rows)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

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
    fn private_flag_roundtrips_and_purge_returns_them() {
        let store = Store::open_in_memory().unwrap();
        let normal = session();
        let mut private = session();
        private.private = true;
        store.insert_session(&normal).unwrap();
        store.insert_session(&private).unwrap();

        assert!(!store.get_session(&normal.id).unwrap().private);
        assert!(store.get_session(&private.id).unwrap().private);

        // A queued message in the private session still carries text.
        store
            .enqueue(&ka_agent_core::QueueItem::new(
                private.id.clone(),
                "queued private text".into(),
                "telegram",
            ))
            .unwrap();
        assert!(store.next_queued(&private.id).unwrap().is_some());
        // An approval card in the private session embeds tool arguments.
        store
            .insert_approval(&ka_agent_core::Approval::new(
                private.id.clone(),
                ka_agent_core::TurnId::generate(),
                ka_agent_core::ApprovalKind::KaAsk {
                    ask_id: "ask-1".into(),
                    question: "run rm -rf secret-project".into(),
                    detail: None,
                    options: vec!["allow".into(), "deny".into()],
                },
                60,
            ))
            .unwrap();
        assert!(!store.pending_approvals(&private.id).unwrap().is_empty());

        // The purge deletes private queue rows and approval cards, and
        // lists every private session; the caller decides which
        // remnants to destroy.
        let purged = store.private_sessions_purge_queue().unwrap();
        assert_eq!(purged.len(), 1, "only the private session: {purged:?}");
        assert_eq!(purged[0].id, private.id);
        assert!(store.next_queued(&private.id).unwrap().is_none());
        assert!(
            store.pending_approvals(&private.id).unwrap().is_empty(),
            "the private session's approval cards are purged"
        );
    }

    #[test]
    fn sessions_are_found_by_role() {
        let store = Store::open_in_memory().unwrap();
        let admin = session();
        let guest = Session::new_for_role(std::path::PathBuf::from("/tmp/guest"), "guest");
        store.insert_session(&admin).unwrap();
        store.insert_session(&guest).unwrap();

        assert_eq!(store.get_session(&guest.id).unwrap().role, "guest");
        let found = store.latest_session_for_role("guest").unwrap().unwrap();
        assert_eq!(found.id, guest.id);
        assert_eq!(
            store.latest_session_for_role("admin").unwrap().unwrap().id,
            admin.id
        );
        assert!(store.latest_session_for_role("nobody").unwrap().is_none());
        // The unfiltered listing still sees both.
        assert_eq!(store.list_recent_sessions(10).unwrap().len(), 2);
    }
}
