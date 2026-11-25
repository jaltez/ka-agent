//! Per-turn submission source (v22). The dream extractor uses it to
//! exclude scheduled-source turns from its input; the turn row itself has
//! no source column.

use super::*;

impl Store {
    /// Record the source that submitted a turn. Idempotent.
    pub fn set_turn_origin(&self, turn: &TurnId, source: &str) -> Result<()> {
        let conn = self.lock();
        conn.execute(
            "INSERT OR REPLACE INTO turn_origins(turn_id, source, created_ms)
             VALUES (?1, ?2, ?3)",
            params![turn.as_str(), source, ka_agent_core::now_ms()],
        )?;
        Ok(())
    }

    /// The source that submitted a turn (`None` for rows written before
    /// v22 or for a turn that never reached the insert point).
    pub fn turn_origin(&self, turn: &TurnId) -> Result<Option<String>> {
        let conn = self.lock();
        conn.query_row(
            "SELECT source FROM turn_origins WHERE turn_id = ?1",
            params![turn.as_str()],
            |r| r.get(0),
        )
        .optional()
        .map_err(StoreError::from)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[test]
    fn origin_roundtrip_and_update() {
        let store = Store::open_in_memory().unwrap();
        let turn = TurnId("turn_origin_test".into());
        assert_eq!(store.turn_origin(&turn).unwrap(), None);
        store.set_turn_origin(&turn, "console").unwrap();
        assert_eq!(
            store.turn_origin(&turn).unwrap().as_deref(),
            Some("console")
        );
        store.set_turn_origin(&turn, "cron:nightly").unwrap();
        assert_eq!(
            store.turn_origin(&turn).unwrap().as_deref(),
            Some("cron:nightly")
        );
    }
}
