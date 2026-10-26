//! Memory candidate review queue (`/memory review`).
//! Split from lib.rs (moves only).

use super::*;

impl Store {
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
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[test]
    fn candidates_review_lifecycle() {
        let store = Store::open_in_memory().unwrap();
        let mem = MemoryId::generate();
        store
            .add_candidate(&mem, "session", "prefers vim", Some("turn-9"))
            .unwrap();
        assert_eq!(store.pending_candidates().unwrap(), vec![mem.clone()]);
        assert_eq!(store.pending_candidates_full().unwrap().len(), 1);
        store.set_candidate_status(&mem, "accepted").unwrap();
        assert!(store.pending_candidates().unwrap().is_empty());
    }
}
