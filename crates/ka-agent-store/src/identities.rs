//! Channel identity links (SEED.md §8.1, Phase 6).
//! Split from lib.rs (moves only).

use super::*;

impl Store {
    /// Link a channel identity to this instance's operator. Idempotent:
    /// an existing link is returned as `false` (unchanged).
    pub fn link_identity(
        &self,
        channel: &str,
        address: &str,
        via: &str,
    ) -> Result<bool, StoreError> {
        let conn = self.lock();
        let changed = conn.execute(
            "INSERT OR IGNORE INTO identities(channel, address, linked_ms, via)
             VALUES (?1, ?2, ?3, ?4)",
            params![channel, address, ka_agent_core::now_ms(), via],
        )?;
        Ok(changed == 1)
    }

    /// Unlink one identity; `true` when a row was removed.
    pub fn unlink_identity(&self, channel: &str, address: &str) -> Result<bool, StoreError> {
        let conn = self.lock();
        let changed = conn.execute(
            "DELETE FROM identities WHERE channel = ?1 AND address = ?2",
            params![channel, address],
        )?;
        Ok(changed == 1)
    }

    /// Every linked identity, oldest link first.
    pub fn list_identities(&self) -> Result<Vec<IdentityRow>, StoreError> {
        let conn = self.lock();
        let mut stmt =
            conn.prepare("SELECT channel, address, linked_ms, via FROM identities ORDER BY linked_ms, channel, address")?;
        let rows = stmt.query_map([], |r| {
            Ok(IdentityRow {
                channel: r.get(0)?,
                address: r.get(1)?,
                linked_ms: r.get(2)?,
                via: r.get(3)?,
            })
        })?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    /// Whether a channel identity is linked to the operator.
    pub fn is_linked(&self, channel: &str, address: &str) -> Result<bool, StoreError> {
        let conn = self.lock();
        let n: i64 = conn.query_row(
            "SELECT COUNT(*) FROM identities WHERE channel = ?1 AND address = ?2",
            params![channel, address],
            |r| r.get(0),
        )?;
        Ok(n > 0)
    }

    /// Unknown contacts that tried to reach a channel, newest first:
    /// `(actor, last_ms, count)` grouped over the `*.unauthorized` audit
    /// rows (actor is `<channel>:<address>`).
    pub fn unauthorized_contacts(&self, limit: usize) -> Result<Vec<(String, i64, i64)>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT actor, MAX(at_ms), COUNT(*) FROM audit
             WHERE action IN ('telegram.unauthorized','matrix.unauthorized')
             GROUP BY actor ORDER BY 2 DESC LIMIT ?1",
        )?;
        let rows = stmt.query_map([limit as i64], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[test]
    fn identities_link_once_and_list_stably() {
        let store = Store::open_in_memory().unwrap();
        assert!(!store.is_linked("telegram", "12345").unwrap());
        assert!(store.link_identity("telegram", "12345", "cli").unwrap());
        // Re-linking the same pair is a no-op, not an error or an update.
        assert!(
            !store
                .link_identity("telegram", "12345", "config-migration")
                .unwrap()
        );
        assert!(store.link_identity("matrix", "@a:b.c", "cli").unwrap());
        let ids = store.list_identities().unwrap();
        assert_eq!(ids.len(), 2);
        let tg = ids.iter().find(|i| i.channel == "telegram").unwrap();
        assert_eq!(tg.via, "cli", "the first link keeps its provenance");
        assert!(
            ids.iter()
                .any(|i| i.channel == "matrix" && i.address == "@a:b.c")
        );
        assert!(store.is_linked("matrix", "@a:b.c").unwrap());
        // Channel-scoped: the same address on another channel is separate.
        assert!(!store.is_linked("telegram", "@a:b.c").unwrap());
        assert!(store.unlink_identity("matrix", "@a:b.c").unwrap());
        assert!(!store.unlink_identity("matrix", "@a:b.c").unwrap());
        assert!(!store.is_linked("matrix", "@a:b.c").unwrap());
    }
}
