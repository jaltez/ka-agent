//! Session persistence: insert, status transitions, lookup, strand binding.
//! Split from lib.rs (moves only).

use super::*;

impl Store {
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
}
