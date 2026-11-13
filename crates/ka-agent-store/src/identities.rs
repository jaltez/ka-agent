//! Channel identity links (SEED.md §8.1, Phase 6).
//! Split from lib.rs (moves only).

use super::*;

impl Store {
    /// Link a channel identity as `admin` (the pre-RBAC behavior).
    /// Idempotent: an existing link is returned as `false` (unchanged).
    pub fn link_identity(
        &self,
        channel: &str,
        address: &str,
        via: &str,
    ) -> Result<bool, StoreError> {
        self.link_identity_as(channel, address, via, ka_agent_core::ADMIN_ROLE)
    }

    /// Link a channel identity with a role. Idempotent: an existing link is
    /// returned as `false` (unchanged; use [`Self::set_identity_role`]).
    pub fn link_identity_as(
        &self,
        channel: &str,
        address: &str,
        via: &str,
        role: &str,
    ) -> Result<bool, StoreError> {
        self.link_identity_to_user(channel, address, via, role, &crate::user_for_role(role))
    }

    /// Link an identity to an explicit user (and, for display, a role).
    /// The role-named user is created when missing so the identity always
    /// resolves to a person.
    pub fn link_identity_to_user(
        &self,
        channel: &str,
        address: &str,
        via: &str,
        role: &str,
        username: &str,
    ) -> Result<bool, StoreError> {
        let conn = self.lock();
        conn.execute(
            "INSERT OR IGNORE INTO users(username, display_name, role, created_ms, created_by)
             VALUES (?1, ?1, ?2, ?3, 'identity-link')",
            params![username, role, ka_agent_core::now_ms()],
        )?;
        let changed = conn.execute(
            "INSERT OR IGNORE INTO identities(channel, address, linked_ms, via, role, username)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                channel,
                address,
                ka_agent_core::now_ms(),
                via,
                role,
                username
            ],
        )?;
        Ok(changed == 1)
    }

    /// Reassign an identity's role; `Ok(false)` when the row does not
    /// exist or already carries that role.
    /// Reassign an identity's role. In the user model this also rebinds the
    /// identity to the user that role names (`admin` → `operator`, else the
    /// role's own user, created on demand) — use `set_user_role` to change
    /// a person's permissions across all their identities instead.
    pub fn set_identity_role(
        &self,
        channel: &str,
        address: &str,
        role: &str,
    ) -> Result<bool, StoreError> {
        let username = crate::user_for_role(role);
        let conn = self.lock();
        conn.execute(
            "INSERT OR IGNORE INTO users(username, display_name, role, created_ms, created_by)
             VALUES (?1, ?1, ?2, ?3, 'identity-role')",
            params![username, role, ka_agent_core::now_ms()],
        )?;
        let changed = conn.execute(
            "UPDATE identities SET role = ?3, username = ?4
             WHERE channel = ?1 AND address = ?2 AND (role != ?3 OR username IS NULL OR username != ?4)",
            params![channel, address, role, username],
        )?;
        Ok(changed == 1)
    }

    /// The role one linked identity acts as (`None` when unlinked).
    pub fn identity_role(
        &self,
        channel: &str,
        address: &str,
    ) -> Result<Option<String>, StoreError> {
        let conn = self.lock();
        conn.query_row(
            "SELECT role FROM identities WHERE channel = ?1 AND address = ?2",
            params![channel, address],
            |r| r.get(0),
        )
        .optional()
        .map_err(StoreError::from)
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
        let mut stmt = conn.prepare(
            "SELECT channel, address, linked_ms, via, role, username FROM identities \
             ORDER BY linked_ms, channel, address",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(IdentityRow {
                channel: r.get(0)?,
                address: r.get(1)?,
                linked_ms: r.get(2)?,
                via: r.get(3)?,
                role: r.get(4)?,
                username: r.get(5)?,
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
