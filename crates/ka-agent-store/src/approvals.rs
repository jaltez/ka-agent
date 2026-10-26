//! Approval persistence: insert, list, decide, expire.
//! Split from lib.rs (moves only).

use super::*;

impl Store {
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
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use ka_agent_core::ApprovalKind;

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
}
