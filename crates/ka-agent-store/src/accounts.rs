//! Accounts: web sessions and admin-minted invites (ADR-0004, D8).
//!
//! Sessions are table-backed so they survive a daemon restart (the old
//! in-memory map did not). Only the SHA-256 of a cookie/ invite code is
//! stored: a leaked database cannot be replayed against the panel.

use super::*;

/// One web session row.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WebSessionRow {
    pub token_hash: String,
    pub username: String,
    pub created_ms: i64,
    pub expires_ms: i64,
}

/// One invite row.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InviteRow {
    pub code_hash: String,
    pub role: String,
    /// `None` = the code creates a new account; `Some(name)` = it resets
    /// that account's password.
    pub username: Option<String>,
    pub groups: Vec<String>,
    pub created_ms: i64,
    pub expires_ms: i64,
    pub used_ms: Option<i64>,
    pub created_by: String,
}

/// What an invite redemption lookup found.
#[derive(Debug, Clone, PartialEq)]
pub enum InviteVerdict {
    /// Redeemable now.
    Valid(InviteRow),
    /// No such code.
    Unknown,
    /// Past its TTL.
    Expired(InviteRow),
    /// Already redeemed.
    Used(InviteRow),
}

fn row_to_web_session(r: &rusqlite::Row<'_>) -> rusqlite::Result<WebSessionRow> {
    Ok(WebSessionRow {
        token_hash: r.get(0)?,
        username: r.get(1)?,
        created_ms: r.get(2)?,
        expires_ms: r.get(3)?,
    })
}

fn row_to_invite(r: &rusqlite::Row<'_>) -> rusqlite::Result<InviteRow> {
    let groups: Option<String> = r.get(2)?;
    Ok(InviteRow {
        code_hash: r.get(0)?,
        role: r.get(1)?,
        username: r.get(7)?,
        groups: groups
            .and_then(|g| serde_json::from_str::<Vec<String>>(&g).ok())
            .unwrap_or_default(),
        created_ms: r.get(3)?,
        expires_ms: r.get(4)?,
        used_ms: r.get(5)?,
        created_by: r.get(6)?,
    })
}

const INVITE_COLS: &str =
    "code_hash, role, groups, created_ms, expires_ms, used_ms, created_by, username";

impl Store {
    // ---- web sessions --------------------------------------------------

    pub fn insert_web_session(
        &self,
        token_hash: &str,
        username: &str,
        created_ms: i64,
        expires_ms: i64,
    ) -> Result<()> {
        let conn = self.lock();
        conn.execute(
            "INSERT OR REPLACE INTO web_sessions(token_hash, username, created_ms, expires_ms)
             VALUES (?1, ?2, ?3, ?4)",
            params![token_hash, username, created_ms, expires_ms],
        )?;
        Ok(())
    }

    /// The user behind a live cookie, sweeping every expired row first (the
    /// sweep is cheap and keeps the table from growing without a timer).
    pub fn web_session_user(&self, token_hash: &str, now_ms: i64) -> Result<Option<String>> {
        let conn = self.lock();
        conn.execute("DELETE FROM web_sessions WHERE expires_ms <= ?1", params![now_ms])?;
        conn.query_row(
            "SELECT username FROM web_sessions WHERE token_hash = ?1",
            params![token_hash],
            |r| r.get(0),
        )
        .optional()
        .map_err(StoreError::from)
    }

    /// Delete every expired session; returns how many rows went away.
    pub fn sweep_web_sessions(&self, now_ms: i64) -> Result<usize> {
        let conn = self.lock();
        Ok(conn.execute("DELETE FROM web_sessions WHERE expires_ms <= ?1", params![now_ms])?)
    }

    pub fn delete_web_session(&self, token_hash: &str) -> Result<bool> {
        let conn = self.lock();
        Ok(conn.execute(
            "DELETE FROM web_sessions WHERE token_hash = ?1",
            params![token_hash],
        )? == 1)
    }

    pub fn list_web_sessions(&self) -> Result<Vec<WebSessionRow>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT token_hash, username, created_ms, expires_ms FROM web_sessions
             ORDER BY username, created_ms",
        )?;
        let rows = stmt.query_map([], row_to_web_session)?;
        collect_rows(rows)
    }

    // ---- invites -------------------------------------------------------

    pub fn insert_invite(&self, invite: &InviteRow) -> Result<()> {
        let groups = if invite.groups.is_empty() {
            None
        } else {
            Some(serde_json::to_string(&invite.groups).unwrap_or_else(|_| "[]".into()))
        };
        let conn = self.lock();
        conn.execute(
            "INSERT INTO invites(code_hash, role, groups, created_ms, expires_ms, used_ms, created_by, username)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                invite.code_hash,
                invite.role,
                groups,
                invite.created_ms,
                invite.expires_ms,
                invite.used_ms,
                invite.created_by,
                invite.username
            ],
        )?;
        Ok(())
    }

    /// Look an invite up without consuming it.
    pub fn invite_verdict(&self, code_hash: &str, now_ms: i64) -> Result<InviteVerdict> {
        let conn = self.lock();
        let row = conn
            .query_row(
                &format!("SELECT {INVITE_COLS} FROM invites WHERE code_hash = ?1"),
                params![code_hash],
                row_to_invite,
            )
            .optional()?;
        Ok(match row {
            None => InviteVerdict::Unknown,
            Some(row) if row.used_ms.is_some() => InviteVerdict::Used(row),
            Some(row) if row.expires_ms <= now_ms => InviteVerdict::Expired(row),
            Some(row) => InviteVerdict::Valid(row),
        })
    }

    /// Consume an invite exactly once (compare-and-swap on `used_ms`).
    pub fn consume_invite(&self, code_hash: &str, now_ms: i64) -> Result<bool> {
        let conn = self.lock();
        let changed = conn.execute(
            "UPDATE invites SET used_ms = ?2 WHERE code_hash = ?1 AND used_ms IS NULL",
            params![code_hash, now_ms],
        )?;
        Ok(changed == 1)
    }

    pub fn list_invites(&self) -> Result<Vec<InviteRow>> {
        let conn = self.lock();
        let mut stmt =
            conn.prepare(&format!("SELECT {INVITE_COLS} FROM invites ORDER BY created_ms DESC"))?;
        let rows = stmt.query_map([], row_to_invite)?;
        collect_rows(rows)
    }

    pub fn delete_invite(&self, code_hash: &str) -> Result<bool> {
        let conn = self.lock();
        Ok(conn.execute(
            "DELETE FROM invites WHERE code_hash = ?1",
            params![code_hash],
        )? == 1)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    /// The migration creates `operator` on every fresh store.
    fn operator(store: &Store) {
        assert!(store.get_user("operator").unwrap().is_some());
    }

    #[test]
    fn web_sessions_resolve_and_expire() {
        let store = Store::open_in_memory().unwrap();
        operator(&store);
        store.insert_web_session("h1", "operator", 1_000, 2_000).unwrap();
        store.insert_web_session("h2", "operator", 1_000, 5_000).unwrap();
        assert_eq!(
            store.web_session_user("h1", 1_500).unwrap().as_deref(),
            Some("operator")
        );
        // Reading past the expiry sweeps the row and answers nothing.
        assert_eq!(store.web_session_user("h1", 2_000).unwrap(), None);
        assert_eq!(store.web_session_user("h2", 2_000).unwrap().as_deref(), Some("operator"));
        assert_eq!(store.list_web_sessions().unwrap().len(), 1);
        assert_eq!(store.delete_web_sessions_for("operator").unwrap(), 1);
        assert_eq!(store.web_session_user("h2", 2_500).unwrap(), None);
        assert!(!store.delete_web_session("h2").unwrap());
    }

    #[test]
    fn invites_are_single_use_with_a_ttl() {
        let store = Store::open_in_memory().unwrap();
        operator(&store);
        let invite = InviteRow {
            code_hash: "c1".into(),
            role: "member".into(),
            username: None,
            groups: vec!["dev".into()],
            created_ms: 1_000,
            expires_ms: 2_000,
            used_ms: None,
            created_by: "operator".into(),
        };
        store.insert_invite(&invite).unwrap();
        assert_eq!(store.list_invites().unwrap(), vec![invite.clone()]);

        assert_eq!(store.invite_verdict("nope", 1_500).unwrap(), InviteVerdict::Unknown);
        match store.invite_verdict("c1", 1_500).unwrap() {
            InviteVerdict::Valid(row) => {
                assert_eq!(row.groups, vec!["dev"]);
                assert_eq!(row.role, "member");
            }
            other => panic!("{other:?}"),
        }
        assert!(store.consume_invite("c1", 1_500).unwrap());
        assert!(!store.consume_invite("c1", 1_600).unwrap(), "single use");
        assert!(matches!(
            store.invite_verdict("c1", 1_600).unwrap(),
            InviteVerdict::Used(_)
        ));

        let stale = InviteRow {
            code_hash: "c2".into(),
            used_ms: None,
            expires_ms: 2_000,
            ..invite
        };
        store.insert_invite(&stale).unwrap();
        assert!(matches!(
            store.invite_verdict("c2", 2_000).unwrap(),
            InviteVerdict::Expired(_)
        ));
        assert!(store.delete_invite("c2").unwrap());
        assert_eq!(store.list_invites().unwrap().len(), 1);
    }
}
