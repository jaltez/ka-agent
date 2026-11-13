//! Groups (ADR-0004): the membership layer that governs resources —
//! which models a user may reach and which KBs a user can read.
//!
//! A group carries at most a model allowlist and a default model. The user's
//! effective model policy is the union across their groups: a group with no
//! allowlist contributes every catalog model, so a user in any unrestricted
//! group is unrestricted.

use super::*;

/// One group row; `models_allow` is the parsed JSON array.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GroupRow {
    pub name: String,
    /// Model globs (`anthropic/*`); empty = every catalog model.
    pub models_allow: Vec<String>,
    /// `openai/gpt-5.2`; `None` = the profile default.
    pub models_default: Option<String>,
}

fn parse_globs(text: Option<&str>) -> Vec<String> {
    text.and_then(|t| serde_json::from_str::<Vec<String>>(t).ok())
        .unwrap_or_default()
}

fn row_to_group(r: &rusqlite::Row<'_>) -> rusqlite::Result<GroupRow> {
    let allow: Option<String> = r.get(1)?;
    Ok(GroupRow {
        name: r.get(0)?,
        models_allow: parse_globs(allow.as_deref()),
        models_default: r.get(2)?,
    })
}

impl Store {
    /// Create a group if missing, never touching an existing group's policy
    /// (every "ensure this group exists" path — invites, membership adds,
    /// KB grants — must use this, not [`Self::upsert_group`]).
    pub fn ensure_group(&self, name: &str) -> Result<bool> {
        let conn = self.lock();
        let created = conn.execute(
            "INSERT OR IGNORE INTO groups(name) VALUES (?1)",
            params![name],
        )?;
        Ok(created == 1)
    }

    /// Create or replace one group's policy.
    pub fn upsert_group(
        &self,
        name: &str,
        models_allow: &[String],
        models_default: Option<&str>,
    ) -> Result<()> {
        let allow_json = if models_allow.is_empty() {
            None
        } else {
            Some(serde_json::to_string(models_allow).unwrap_or_else(|_| "[]".into()))
        };
        let conn = self.lock();
        conn.execute(
            "INSERT INTO groups(name, models_allow, models_default) VALUES (?1, ?2, ?3)
             ON CONFLICT(name) DO UPDATE SET models_allow = ?2, models_default = ?3",
            params![name, allow_json, models_default],
        )?;
        Ok(())
    }

    pub fn get_group(&self, name: &str) -> Result<Option<GroupRow>> {
        let conn = self.lock();
        conn.query_row(
            "SELECT name, models_allow, models_default FROM groups WHERE name = ?1",
            params![name],
            row_to_group,
        )
        .optional()
        .map_err(StoreError::from)
    }

    pub fn list_groups(&self) -> Result<Vec<GroupRow>> {
        let conn = self.lock();
        let mut stmt =
            conn.prepare("SELECT name, models_allow, models_default FROM groups ORDER BY name")?;
        let rows = stmt.query_map([], row_to_group)?;
        collect_rows(rows)
    }

    /// Delete a group and its memberships/kb grants; `true` when it existed.
    pub fn delete_group(&self, name: &str) -> Result<bool> {
        let mut conn = self.lock();
        let tx = conn.transaction()?;
        tx.execute("DELETE FROM users_groups WHERE group_name = ?1", params![name])?;
        tx.execute("DELETE FROM kb_access WHERE group_name = ?1", params![name])?;
        tx.execute("DELETE FROM agent_access WHERE group_name = ?1", params![name])?;
        let changed = tx.execute("DELETE FROM groups WHERE name = ?1", params![name])?;
        tx.commit()?;
        Ok(changed == 1)
    }

    /// Replace one user's group membership.
    pub fn set_user_groups(&self, username: &str, groups: &[String]) -> Result<()> {
        let mut conn = self.lock();
        let tx = conn.transaction()?;
        tx.execute("DELETE FROM users_groups WHERE username = ?1", params![username])?;
        for group in groups {
            tx.execute(
                "INSERT OR IGNORE INTO users_groups(username, group_name) VALUES (?1, ?2)",
                params![username, group],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn add_user_group(&self, username: &str, group: &str) -> Result<bool> {
        let conn = self.lock();
        let changed = conn.execute(
            "INSERT OR IGNORE INTO users_groups(username, group_name) VALUES (?1, ?2)",
            params![username, group],
        )?;
        Ok(changed == 1)
    }

    pub fn remove_user_group(&self, username: &str, group: &str) -> Result<bool> {
        let conn = self.lock();
        let changed = conn.execute(
            "DELETE FROM users_groups WHERE username = ?1 AND group_name = ?2",
            params![username, group],
        )?;
        Ok(changed == 1)
    }

    /// A user's groups, alphabetical.
    pub fn groups_of(&self, username: &str) -> Result<Vec<String>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT group_name FROM users_groups WHERE username = ?1 ORDER BY group_name",
        )?;
        let rows = stmt.query_map(params![username], |r| r.get::<_, String>(0))?;
        collect_rows(rows)
    }

    /// A group's members, alphabetical.
    pub fn group_members(&self, group: &str) -> Result<Vec<String>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT username FROM users_groups WHERE group_name = ?1 ORDER BY username",
        )?;
        let rows = stmt.query_map(params![group], |r| r.get::<_, String>(0))?;
        collect_rows(rows)
    }

    /// A user's effective model policy: `(allow globs, default model)`.
    /// `allow` is empty when any of the user's groups is unrestricted (or
    /// the user has no groups at all); `default` is the first non-NULL
    /// `models_default` scanning the groups alphabetically.
    pub fn model_policy_for(&self, username: &str) -> Result<(Vec<String>, Option<String>)> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT g.models_allow, g.models_default
             FROM users_groups ug JOIN groups g ON g.name = ug.group_name
             WHERE ug.username = ?1 ORDER BY g.name",
        )?;
        let rows = stmt.query_map(params![username], |r| {
            Ok((
                r.get::<_, Option<String>>(0)?,
                r.get::<_, Option<String>>(1)?,
            ))
        })?;
        let mut allow: Vec<String> = Vec::new();
        let mut unrestricted = false;
        let mut default = None;
        let mut any_group = false;
        for row in rows {
            let (globs, dflt) = row?;
            any_group = true;
            let globs = parse_globs(globs.as_deref());
            if globs.is_empty() {
                unrestricted = true;
            } else {
                for g in globs {
                    if !allow.contains(&g) {
                        allow.push(g);
                    }
                }
            }
            if default.is_none()
                && let Some(d) = dflt
                && !d.trim().is_empty()
            {
                default = Some(d);
            }
        }
        if !any_group || unrestricted {
            allow.clear();
        }
        Ok((allow, default))
    }
}

/// Whether `model` (`vendor/model`) matches one allow glob. `*` matches any
/// run of characters; a bare `vendor` or `vendor/` means `vendor/*`.
pub fn model_matches(glob: &str, model: &str) -> bool {
    let glob = glob.trim();
    if glob.is_empty() {
        return false;
    }
    let glob = if glob.ends_with('/') {
        format!("{glob}*")
    } else {
        glob.to_string()
    };
    // Simple wildcard matcher over the two strings (only `*`, no regex): on
    // a mismatch the last `*` consumes one more character of the subject.
    let g: Vec<char> = glob.chars().collect();
    let m: Vec<char> = model.chars().collect();
    let (mut gi, mut mi) = (0usize, 0usize);
    let mut star: Option<(usize, usize)> = None;
    while mi < m.len() {
        if gi < g.len() && g[gi] == m[mi] {
            gi += 1;
            mi += 1;
        } else if gi < g.len() && g[gi] == '*' {
            star = Some((gi, mi));
            gi += 1;
        } else if let Some((sg, sm)) = star {
            // The star eats one more subject character and the pattern
            // resumes right after it; the bound always advances, so the
            // loop terminates.
            let next = sm + 1;
            star = Some((sg, next));
            gi = sg + 1;
            mi = next;
        } else {
            return false;
        }
    }
    while gi < g.len() && g[gi] == '*' {
        gi += 1;
    }
    gi == g.len()
}

/// Whether `model` is inside an allowlist (empty = everything allowed).
pub fn model_allowed(allow: &[String], model: &str) -> bool {
    allow.is_empty() || allow.iter().any(|g| model_matches(g, model))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[test]
    fn glob_matching() {
        assert!(model_matches("anthropic/*", "anthropic/claude"));
        assert!(model_matches("anthropic/", "anthropic/claude"));
        assert!(model_matches("*", "openai/gpt"));
        assert!(model_matches("openai/gpt-5*", "openai/gpt-5.2"));
        assert!(!model_matches("openai/*", "anthropic/claude"));
        assert!(!model_matches("openai/gpt-4", "openai/gpt-45"));
        assert!(model_matches("openai/gpt-4*", "openai/gpt-45"));
        assert!(!model_matches("", "openai/gpt"));
        assert!(model_allowed(&[], "anything/at-all"));
        assert!(!model_allowed(&["anthropic/*".into()], "openai/gpt"));
    }

    #[test]
    fn membership_and_policy_union() {
        let store = Store::open_in_memory().unwrap();
        store
            .upsert_group("dev", &["anthropic/*".into()], Some("anthropic/claude"))
            .unwrap();
        store.upsert_group("ops", &["openai/*".into()], None).unwrap();
        store.upsert_group("all", &[], None).unwrap();
        assert_eq!(store.list_groups().unwrap().len(), 3);
        assert_eq!(
            store.get_group("dev").unwrap().unwrap().models_allow,
            vec!["anthropic/*".to_string()]
        );

        for name in ["ana", "bob"] {
            store
                .create_user(&crate::users::UserRow {
                    username: name.into(),
                    display_name: name.into(),
                    pw_hash: None,
                    role: "member".into(),
                    quota_daily_usd: None,
                    disabled: false,
                    created_ms: 1,
                    created_by: Some("test".into()),
                })
                .unwrap();
        }
        store.set_user_groups("ana", &["dev".into(), "ops".into()]).unwrap();
        assert_eq!(store.groups_of("ana").unwrap(), vec!["dev", "ops"]);
        let (allow, dflt) = store.model_policy_for("ana").unwrap();
        assert_eq!(allow, vec!["anthropic/*".to_string(), "openai/*".to_string()]);
        assert_eq!(dflt.as_deref(), Some("anthropic/claude"));

        // Any unrestricted group (or no group) means every catalog model.
        store.add_user_group("ana", "all").unwrap();
        let (allow, _) = store.model_policy_for("ana").unwrap();
        assert!(allow.is_empty(), "{allow:?}");
        store.set_user_groups("bob", &[]).unwrap();
        assert!(store.model_policy_for("bob").unwrap().0.is_empty());

        assert_eq!(store.group_members("dev").unwrap(), vec!["ana"]);
        store.remove_user_group("ana", "dev").unwrap();
        assert!(store.group_members("dev").unwrap().is_empty());
        assert!(store.delete_group("dev").unwrap());
        assert!(store.get_group("dev").unwrap().is_none());
        assert!(!store.delete_group("dev").unwrap());
    }
}
