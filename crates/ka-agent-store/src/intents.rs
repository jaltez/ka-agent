//! External-write intents (SEED.md §7.3): idempotency records for
//! side-effecting API calls.
//! Split from lib.rs (moves only).

use super::*;

impl Store {
    /// Insert a new external-write intent (status `proposed`).
    pub fn insert_intent(&self, i: &IntentRow) -> Result<()> {
        let conn = self.lock();
        conn.execute(
            "INSERT INTO intents(id, tool, args_hash, status, external_ref, summary, created_ms, updated_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                i.id, i.tool, i.args_hash, i.status,
                i.external_ref, i.summary, i.created_ms, i.updated_ms
            ],
        )?;
        Ok(())
    }

    /// Update an intent's status (and optionally its external_ref).
    pub fn update_intent_status(
        &self,
        id: &str,
        status: &str,
        external_ref: Option<&str>,
    ) -> Result<()> {
        let conn = self.lock();
        let changed = conn.execute(
            "UPDATE intents SET status = ?2, external_ref = COALESCE(?3, external_ref), updated_ms = ?4
             WHERE id = ?1",
            params![id, status, external_ref, ka_agent_core::now_ms()],
        )?;
        if changed == 0 {
            return Err(StoreError::NotFound(format!("intent {id}")));
        }
        Ok(())
    }

    /// The most recent non-failed intent for a tool + args hash. This is
    /// the idempotency check for external writes (SEED.md §7.3): a retried
    /// call with identical arguments must find the earlier effect here and
    /// refuse to write again. Failed attempts stay retryable.
    pub fn active_intent_by_hash(&self, tool: &str, args_hash: &str) -> Result<Option<IntentRow>> {
        let conn = self.lock();
        conn.query_row(
            "SELECT id, tool, args_hash, status, external_ref, summary, created_ms, updated_ms
             FROM intents
             WHERE tool = ?1 AND args_hash = ?2 AND status != 'failed'
             ORDER BY created_ms DESC LIMIT 1",
            params![tool, args_hash],
            |r| {
                Ok(IntentRow {
                    id: r.get(0)?,
                    tool: r.get(1)?,
                    args_hash: r.get(2)?,
                    status: r.get(3)?,
                    external_ref: r.get(4)?,
                    summary: r.get(5)?,
                    created_ms: r.get(6)?,
                    updated_ms: r.get(7)?,
                })
            },
        )
        .optional()
        .map_err(|e| e.into())
    }

    /// Recent intents (for reporting).
    pub fn recent_intents(&self, limit: usize) -> Result<Vec<IntentRow>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT id, tool, args_hash, status, external_ref, summary, created_ms, updated_ms
             FROM intents ORDER BY created_ms DESC LIMIT ?1",
        )?;
        let rows = stmt.query_map(params![limit as i64], |r| {
            Ok(IntentRow {
                id: r.get(0)?,
                tool: r.get(1)?,
                args_hash: r.get(2)?,
                status: r.get(3)?,
                external_ref: r.get(4)?,
                summary: r.get(5)?,
                created_ms: r.get(6)?,
                updated_ms: r.get(7)?,
            })
        })?;
        collect_rows(rows)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[test]
    fn intent_lifecycle() {
        let store = Store::open_in_memory().unwrap();
        let now = ka_agent_core::now_ms();
        let intent = IntentRow {
            id: "int_test1".to_string(),
            tool: "github_issue_comment".to_string(),
            args_hash: "abc123".to_string(),
            status: "executing".to_string(),
            external_ref: None,
            summary: Some("comment on #42 in owner/repo".to_string()),
            created_ms: now,
            updated_ms: now,
        };
        store.insert_intent(&intent).unwrap();
        // executing → done with external_ref
        store
            .update_intent_status(
                "int_test1",
                "done",
                Some("https://api.github.com/repos/o/r/issues/comments/12345"),
            )
            .unwrap();
        let intents = store.recent_intents(10).unwrap();
        assert_eq!(intents.len(), 1);
        assert_eq!(intents[0].status, "done");
        assert!(
            intents[0]
                .external_ref
                .as_deref()
                .unwrap_or("")
                .contains("12345")
        );
        // failed path
        let intent2 = IntentRow {
            id: "int_test2".to_string(),
            tool: "github_pull_review".to_string(),
            args_hash: "def456".to_string(),
            status: "executing".to_string(),
            external_ref: None,
            summary: None,
            created_ms: now,
            updated_ms: now,
        };
        store.insert_intent(&intent2).unwrap();
        store
            .update_intent_status("int_test2", "failed", None)
            .unwrap();
        let intents = store.recent_intents(10).unwrap();
        assert_eq!(intents.len(), 2);
        assert!(intents.iter().any(|i| i.status == "failed"));
    }

    #[test]
    fn active_intent_by_hash_is_the_idempotency_key() {
        let store = Store::open_in_memory().unwrap();
        let now = ka_agent_core::now_ms();
        let row = IntentRow {
            id: "int_dup".to_string(),
            tool: "github_issue_comment".to_string(),
            args_hash: "cafe01".to_string(),
            status: "executing".to_string(),
            external_ref: None,
            summary: None,
            created_ms: now,
            updated_ms: now,
        };
        store.insert_intent(&row).unwrap();
        // Same tool+hash while executing/succeeded: found (refuse retry).
        let hit = store
            .active_intent_by_hash("github_issue_comment", "cafe01")
            .unwrap()
            .unwrap();
        assert_eq!(hit.id, "int_dup");
        // Different tool or hash: not found.
        assert!(
            store
                .active_intent_by_hash("github_pull_review", "cafe01")
                .unwrap()
                .is_none()
        );
        assert!(
            store
                .active_intent_by_hash("github_issue_comment", "other")
                .unwrap()
                .is_none()
        );
        // Failed attempts stay retryable.
        store
            .update_intent_status("int_dup", "failed", None)
            .unwrap();
        assert!(
            store
                .active_intent_by_hash("github_issue_comment", "cafe01")
                .unwrap()
                .is_none()
        );
    }
}
