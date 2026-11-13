//! Users (ADR-0004): the person records the team edition routes to.
//!
//! A channel identity resolves to a user; the user's `role` is the
//! permission set the runtime enforces. Nothing else here knows about
//! channels: this module is the identity → user link plus the account
//! columns (password hash, daily quota, disabled flag).

use super::*;

/// One user row.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UserRow {
    pub username: String,
    pub display_name: String,
    /// argon2id PHC string; `None` until an invite is redeemed.
    pub pw_hash: Option<String>,
    /// Names a `[[role]]` in the config (`admin` allowed).
    pub role: String,
    /// Daily USD cap for this user; `None` = the role/profile budget.
    pub quota_daily_usd: Option<f64>,
    pub disabled: bool,
    pub created_ms: i64,
    pub created_by: Option<String>,
}

/// A username: `[a-z0-9][a-z0-9._-]{0,63}` (also used for group names).
pub fn valid_username(name: &str) -> bool {
    let mut chars = name.chars();
    match chars.next() {
        Some(c) if c.is_ascii_lowercase() || c.is_ascii_digit() => {}
        _ => return false,
    }
    name.len() <= 64
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '.' | '_' | '-'))
}

fn row_to_user(r: &rusqlite::Row<'_>) -> rusqlite::Result<UserRow> {
    Ok(UserRow {
        username: r.get(0)?,
        display_name: r.get(1)?,
        pw_hash: r.get(2)?,
        role: r.get(3)?,
        quota_daily_usd: r.get(4)?,
        disabled: r.get::<_, i64>(5)? != 0,
        created_ms: r.get(6)?,
        created_by: r.get(7)?,
    })
}

const USER_COLS: &str =
    "username, display_name, pw_hash, role, quota_daily_usd, disabled, created_ms, created_by";

impl Store {
    /// Insert one user; an existing username is refused (use the setters).
    pub fn create_user(&self, u: &UserRow) -> Result<()> {
        let conn = self.lock();
        conn.execute(
            "INSERT INTO users(username, display_name, pw_hash, role, quota_daily_usd, disabled, created_ms, created_by)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                u.username,
                u.display_name,
                u.pw_hash,
                u.role,
                u.quota_daily_usd,
                u.disabled as i64,
                u.created_ms,
                u.created_by
            ],
        )?;
        Ok(())
    }

    /// Insert a user, or leave an existing row untouched (`true` when a row
    /// was created).
    pub fn ensure_user(&self, u: &UserRow) -> Result<bool> {
        let conn = self.lock();
        let changed = conn.execute(
            "INSERT OR IGNORE INTO users(username, display_name, pw_hash, role, quota_daily_usd, disabled, created_ms, created_by)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                u.username,
                u.display_name,
                u.pw_hash,
                u.role,
                u.quota_daily_usd,
                u.disabled as i64,
                u.created_ms,
                u.created_by
            ],
        )?;
        Ok(changed == 1)
    }

    pub fn get_user(&self, username: &str) -> Result<Option<UserRow>> {
        let conn = self.lock();
        conn.query_row(
            &format!("SELECT {USER_COLS} FROM users WHERE username = ?1"),
            params![username],
            row_to_user,
        )
        .optional()
        .map_err(StoreError::from)
    }

    pub fn list_users(&self) -> Result<Vec<UserRow>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(&format!("SELECT {USER_COLS} FROM users ORDER BY username"))?;
        let rows = stmt.query_map([], row_to_user)?;
        collect_rows(rows)
    }

    /// Disable or re-enable an account (offboarding; D10).
    pub fn set_user_disabled(&self, username: &str, disabled: bool) -> Result<bool> {
        let conn = self.lock();
        let changed = conn.execute(
            "UPDATE users SET disabled = ?2 WHERE username = ?1",
            params![username, disabled as i64],
        )?;
        Ok(changed == 1)
    }

    pub fn set_user_role(&self, username: &str, role: &str) -> Result<bool> {
        let conn = self.lock();
        let changed = conn.execute(
            "UPDATE users SET role = ?2 WHERE username = ?1",
            params![username, role],
        )?;
        Ok(changed == 1)
    }

    /// Set (or clear, with `None`) the password hash. The admin's reset
    /// clears it; invite redemption sets it.
    pub fn set_user_password(&self, username: &str, hash: Option<&str>) -> Result<bool> {
        let conn = self.lock();
        let changed = conn.execute(
            "UPDATE users SET pw_hash = ?2 WHERE username = ?1",
            params![username, hash],
        )?;
        Ok(changed == 1)
    }

    pub fn set_user_quota(&self, username: &str, quota: Option<f64>) -> Result<bool> {
        let conn = self.lock();
        let changed = conn.execute(
            "UPDATE users SET quota_daily_usd = ?2 WHERE username = ?1",
            params![username, quota],
        )?;
        Ok(changed == 1)
    }

    /// The user one linked identity resolves to, with the user's role.
    /// `None` for an unlinked identity, an identity without a bound user,
    /// or a disabled account — exactly the set of senders the runtime
    /// refuses.
    pub fn identity_principal(&self, channel: &str, address: &str) -> Result<Option<(String, String)>> {
        let conn = self.lock();
        conn.query_row(
            "SELECT i.username, COALESCE(u.role, i.role)
             FROM identities i
             LEFT JOIN users u ON u.username = i.username
             WHERE i.channel = ?1 AND i.address = ?2
               AND i.username IS NOT NULL
               AND COALESCE(u.disabled, 0) = 0",
            params![channel, address],
            |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)),
        )
        .optional()
        .map_err(StoreError::from)
    }

    /// Bind an identity to an existing user (no role change on the row:
    /// the user's role wins at resolution).
    pub fn bind_identity_user(
        &self,
        channel: &str,
        address: &str,
        username: &str,
    ) -> Result<bool, StoreError> {
        let conn = self.lock();
        let changed = conn.execute(
            "UPDATE identities SET username = ?3 WHERE channel = ?1 AND address = ?2 AND (username IS NULL OR username != ?3)",
            params![channel, address, username],
        )?;
        Ok(changed == 1)
    }

    /// Every identity bound to one user.
    pub fn identities_for_user(&self, username: &str) -> Result<Vec<IdentityRow>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT channel, address, linked_ms, via, role, username FROM identities
             WHERE username = ?1 ORDER BY channel, address",
        )?;
        let rows = stmt.query_map(params![username], identity_row)?;
        collect_rows(rows)
    }

    /// Remove every web session of one user (disable, reset).
    pub fn delete_web_sessions_for(&self, username: &str) -> Result<usize> {
        let conn = self.lock();
        Ok(conn.execute("DELETE FROM web_sessions WHERE username = ?1", params![username])?)
    }
}

fn identity_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<IdentityRow> {
    Ok(IdentityRow {
        channel: r.get(0)?,
        address: r.get(1)?,
        linked_ms: r.get(2)?,
        via: r.get(3)?,
        role: r.get(4)?,
        username: r.get(5)?,
    })
}

/// The user an identity with `role` binds to when none is named: `admin`
/// is always the operator, any other role binds to the user named after it
/// (created on demand — the same rule schema v12 backfills with).
pub fn user_for_role(role: &str) -> String {
    if role == ka_agent_core::ADMIN_ROLE {
        "operator".to_string()
    } else {
        role.to_string()
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    fn user(name: &str, role: &str) -> UserRow {
        UserRow {
            username: name.to_string(),
            display_name: name.to_string(),
            pw_hash: None,
            role: role.to_string(),
            quota_daily_usd: None,
            disabled: false,
            created_ms: 1,
            created_by: Some("test".into()),
        }
    }

    #[test]
    fn usernames_are_lowercase_words() {
        for ok in ["operator", "ana", "ana.b", "dev-1", "x_y", "a1"] {
            assert!(valid_username(ok), "{ok}");
        }
        for bad in ["", "Ana", "-ana", ".ana", "a b", "a/b", &"a".repeat(65)] {
            assert!(!valid_username(bad), "{bad}");
        }
    }

    #[test]
    fn users_crud_and_role_mapping() {
        let store = Store::open_in_memory().unwrap();
        assert!(store.get_user("operator").unwrap().is_some(), "migrated");
        store.create_user(&user("ana", "dev")).unwrap();
        assert!(store.create_user(&user("ana", "dev")).is_err());
        assert!(!store.ensure_user(&user("ana", "dev")).unwrap());
        assert!(store.ensure_user(&user("bob", "dev")).unwrap());
        let ana = store.get_user("ana").unwrap().unwrap();
        assert_eq!(ana.role, "dev");
        assert!(!ana.disabled);

        store.set_user_disabled("ana", true).unwrap();
        assert!(store.get_user("ana").unwrap().unwrap().disabled);
        store.set_user_role("ana", "guest").unwrap();
        assert_eq!(store.get_user("ana").unwrap().unwrap().role, "guest");
        store.set_user_quota("ana", Some(2.5)).unwrap();
        assert_eq!(store.get_user("ana").unwrap().unwrap().quota_daily_usd, Some(2.5));
        store.set_user_password("ana", Some("$argon2id$v=19$...")).unwrap();
        assert!(store.get_user("ana").unwrap().unwrap().pw_hash.is_some());
        store.set_user_password("ana", None).unwrap();
        assert!(store.get_user("ana").unwrap().unwrap().pw_hash.is_none());
        assert_eq!(store.list_users().unwrap().len(), 3, "operator + ana + bob");
        assert_eq!(user_for_role("admin"), "operator");
        assert_eq!(user_for_role("dev"), "dev");
    }

    #[test]
    fn disabled_users_do_not_resolve() {
        let store = Store::open_in_memory().unwrap();
        store.create_user(&user("ana", "dev")).unwrap();
        store
            .link_identity_as("telegram", "42", "cli", "admin")
            .unwrap();
        store
            .link_identity_as("telegram", "43", "cli", "dev")
            .unwrap();
        assert_eq!(
            store.identity_principal("telegram", "42").unwrap(),
            Some(("operator".into(), "admin".into()))
        );
        assert_eq!(
            store.identity_principal("telegram", "43").unwrap(),
            Some(("dev".into(), "dev".into()))
        );
        // Unlinked sender never resolves.
        assert_eq!(store.identity_principal("telegram", "99").unwrap(), None);
        // Disabling the user stops resolution (the runtime's refusal gate).
        store.set_user_disabled("dev", true).unwrap();
        assert_eq!(store.identity_principal("telegram", "43").unwrap(), None);
        store.set_user_disabled("dev", false).unwrap();
        assert!(store.identity_principal("telegram", "43").unwrap().is_some());
    }
}
