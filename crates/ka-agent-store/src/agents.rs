//! Shared agents (M3): a definition (persona, skills, knowledge, model) that
//! several users can chat with. Run state stays per user; only the
//! definition is shared, with Viewer/Editor/Owner access per group and the
//! "always at least one owner" invariant.

use super::*;

/// One shared agent definition.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AgentRow {
    pub name: String,
    pub persona: String,
    /// Model override; `None` = the user's own policy default.
    pub model: Option<String>,
    pub skills: Vec<String>,
    pub kbs: Vec<String>,
    pub owner_username: String,
    pub created_ms: i64,
}

/// One agent's access grant for a group.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AgentAccess {
    pub group_name: String,
    /// `viewer` | `editor` | `owner`.
    pub level: String,
}

/// An agent as one user sees it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AgentView {
    pub agent: AgentRow,
    /// The user's level across their groups (`owner` when they own it).
    pub level: String,
}

fn row_to_agent(r: &rusqlite::Row<'_>) -> rusqlite::Result<AgentRow> {
    let skills: String = r.get(3)?;
    let kbs: String = r.get(4)?;
    Ok(AgentRow {
        name: r.get(0)?,
        persona: r.get(1)?,
        model: r.get(2)?,
        skills: serde_json::from_str(&skills).unwrap_or_default(),
        kbs: serde_json::from_str(&kbs).unwrap_or_default(),
        owner_username: r.get(5)?,
        created_ms: r.get(6)?,
    })
}

impl Store {
    /// Create an agent owned by `owner` (who also becomes an owner through
    /// the `owner_username` column).
    pub fn create_agent(&self, agent: &AgentRow) -> Result<()> {
        let conn = self.lock();
        conn.execute(
            "INSERT INTO agents(name, persona, model, skills, kbs, owner_username, created_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                agent.name,
                agent.persona,
                agent.model,
                serde_json::to_string(&agent.skills).unwrap_or_else(|_| "[]".into()),
                serde_json::to_string(&agent.kbs).unwrap_or_else(|_| "[]".into()),
                agent.owner_username,
                agent.created_ms
            ],
        )?;
        Ok(())
    }

    /// Update a definition (the caller checks the editor/owner level).
    pub fn update_agent(&self, agent: &AgentRow) -> Result<()> {
        let conn = self.lock();
        let changed = conn.execute(
            "UPDATE agents SET persona = ?2, model = ?3, skills = ?4, kbs = ?5 WHERE name = ?1",
            params![
                agent.name,
                agent.persona,
                agent.model,
                serde_json::to_string(&agent.skills).unwrap_or_else(|_| "[]".into()),
                serde_json::to_string(&agent.kbs).unwrap_or_else(|_| "[]".into())
            ],
        )?;
        if changed == 0 {
            return Err(StoreError::NotFound(format!("agent {}", agent.name)));
        }
        Ok(())
    }

    pub fn get_agent(&self, name: &str) -> Result<Option<AgentRow>> {
        let conn = self.lock();
        conn.query_row(
            "SELECT name, persona, model, skills, kbs, owner_username, created_ms
             FROM agents WHERE name = ?1",
            params![name],
            row_to_agent,
        )
        .optional()
        .map_err(StoreError::from)
    }

    pub fn list_agents(&self) -> Result<Vec<AgentRow>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT name, persona, model, skills, kbs, owner_username, created_ms
             FROM agents ORDER BY name",
        )?;
        let rows = stmt.query_map([], row_to_agent)?;
        collect_rows(rows)
    }

    /// Delete an agent (its access rows go with it). The caller enforces
    /// ownership.
    pub fn delete_agent(&self, name: &str) -> Result<bool> {
        let mut conn = self.lock();
        let tx = conn.transaction()?;
        tx.execute("DELETE FROM agent_access WHERE agent_name = ?1", params![name])?;
        let changed = tx.execute("DELETE FROM agents WHERE name = ?1", params![name])?;
        tx.commit()?;
        Ok(changed == 1)
    }

    /// Replace one agent's access grants. Refuses to leave it ownerless.
    pub fn set_agent_access(&self, name: &str, grants: &[AgentAccess]) -> Result<()> {
        let mut conn = self.lock();
        let owner = conn
            .query_row(
                "SELECT owner_username FROM agents WHERE name = ?1",
                params![name],
                |r| r.get::<_, String>(0),
            )
            .optional()?;
        if owner.is_none() {
            return Err(StoreError::NotFound(format!("agent {name}")));
        }
        // The ≥1-owner invariant lives in `owner_username` (always a real
        // user): group grants add viewers/editors, never displace the owner.
        let tx = conn.transaction()?;
        tx.execute("DELETE FROM agent_access WHERE agent_name = ?1", params![name])?;
        for grant in grants {
            if !matches!(grant.level.as_str(), "viewer" | "editor" | "owner") {
                return Err(StoreError::Refused(format!(
                    "agent {name}: access level {:?} must be viewer, editor or owner",
                    grant.level
                )));
            }
            tx.execute(
                "INSERT OR REPLACE INTO agent_access(agent_name, group_name, level) VALUES (?1, ?2, ?3)",
                params![name, grant.group_name, grant.level],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn agent_access(&self, name: &str) -> Result<Vec<AgentAccess>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT group_name, level FROM agent_access WHERE agent_name = ?1 ORDER BY group_name",
        )?;
        let rows = stmt.query_map(params![name], |r| {
            Ok(AgentAccess {
                group_name: r.get(0)?,
                level: r.get(1)?,
            })
        })?;
        collect_rows(rows)
    }

    /// The best access level `username` has on `name` (`None` = no access).
    /// The owner account and role `admin` always count as owners.
    pub fn agent_level(&self, name: &str, username: &str, role: &str) -> Result<Option<String>> {
        let Some(agent) = self.get_agent(name)? else {
            return Ok(None);
        };
        if role == ka_agent_core::ADMIN_ROLE || agent.owner_username == username {
            return Ok(Some("owner".into()));
        }
        let groups = self.groups_of(username)?;
        let mut best: Option<&'static str> = None;
        for grant in self.agent_access(name)? {
            if !groups.contains(&grant.group_name) {
                continue;
            }
            let level = match grant.level.as_str() {
                "owner" => 3,
                "editor" => 2,
                _ => 1,
            };
            let current = match best {
                Some("owner") => 3,
                Some("editor") => 2,
                Some(_) => 1,
                None => 0,
            };
            if level > current {
                best = Some(match level {
                    3 => "owner",
                    2 => "editor",
                    _ => "viewer",
                });
            }
        }
        Ok(best.map(str::to_string))
    }

    /// Every agent `username` may chat with or see, with the best level.
    pub fn agents_for_user(&self, username: &str, role: &str) -> Result<Vec<AgentView>> {
        let mut out = Vec::new();
        for agent in self.list_agents()? {
            if let Some(level) = self.agent_level(&agent.name, username, role)? {
                out.push(AgentView { agent, level });
            }
        }
        Ok(out)
    }

    /// Transfer ownership to another user; the previous owner keeps access
    /// as an editor group member only if a grant exists. Refuses unknown
    /// users and the "no owner" state is impossible by construction.
    pub fn transfer_agent(&self, name: &str, new_owner: &str) -> Result<()> {
        if self.get_user(new_owner)?.is_none() {
            return Err(StoreError::NotFound(format!("user {new_owner}")));
        }
        let conn = self.lock();
        let changed = conn.execute(
            "UPDATE agents SET owner_username = ?2 WHERE name = ?1",
            params![name, new_owner],
        )?;
        if changed == 0 {
            return Err(StoreError::NotFound(format!("agent {name}")));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    fn user(store: &Store, name: &str, role: &str) {
        store
            .create_user(&crate::UserRow {
                username: name.into(),
                display_name: name.into(),
                pw_hash: None,
                role: role.into(),
                quota_daily_usd: None,
                disabled: false,
                created_ms: 1,
                created_by: Some("test".into()),
            })
            .unwrap();
    }

    #[test]
    fn agents_share_by_group_with_an_owner_invariant() {
        let store = Store::open_in_memory().unwrap();
        user(&store, "ana", "member");
        user(&store, "bob", "member");
        store.upsert_group("dev", &[], None).unwrap();
        let agent = AgentRow {
            name: "reviewer".into(),
            persona: "You review code.".into(),
            model: Some("anthropic/claude".into()),
            skills: vec!["review".into()],
            kbs: vec!["docs".into()],
            owner_username: "ana".into(),
            created_ms: 1,
        };
        store.create_agent(&agent).unwrap();
        assert_eq!(store.get_agent("reviewer").unwrap().unwrap(), agent);
        // The owner has owner access by definition; a non-member nothing.
        assert_eq!(
            store.agent_level("reviewer", "ana", "member").unwrap().as_deref(),
            Some("owner")
        );
        assert_eq!(store.agent_level("reviewer", "bob", "member").unwrap(), None);
        // An unknown access level is refused.
        assert!(store
            .set_agent_access(
                "reviewer",
                &[AgentAccess {
                    group_name: "dev".into(),
                    level: "superuser".into()
                }]
            )
            .is_err());
        store
            .set_agent_access(
                "reviewer",
                &[AgentAccess {
                    group_name: "dev".into(),
                    level: "editor".into(),
                }],
            )
            .unwrap();
        store.add_user_group("bob", "dev").unwrap();
        assert_eq!(
            store.agent_level("reviewer", "bob", "member").unwrap().as_deref(),
            Some("editor")
        );
        assert_eq!(store.agents_for_user("bob", "member").unwrap().len(), 1);
        assert_eq!(store.agents_for_user("moe", "member").unwrap().len(), 0);
        // Admin sees everything as owner.
        assert_eq!(
            store.agent_level("reviewer", "operator", "admin").unwrap().as_deref(),
            Some("owner")
        );
        // Transfer keeps the new owner an owner; an unknown user is refused.
        assert!(store.transfer_agent("reviewer", "nobody").is_err());
        store.transfer_agent("reviewer", "bob").unwrap();
        assert_eq!(store.get_agent("reviewer").unwrap().unwrap().owner_username, "bob");
        assert_eq!(
            store.agent_level("reviewer", "bob", "member").unwrap().as_deref(),
            Some("owner")
        );
        assert!(store.delete_agent("reviewer").unwrap());
        assert!(store.get_agent("reviewer").unwrap().is_none());
    }
}
