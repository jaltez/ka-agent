//! Knowledge bases (ADR-0004, D6): a KB is a named document set with
//! per-group access. Documents are text-like files kept in SQLite with an
//! FTS5 mirror; retrieval is snippet search with a citation path.
//!
//! Access model: a KB is readable by the members of `kb_access`'s groups,
//! by its owner, and by the admin role. No per-document ACLs exist by
//! design (roadmap D6) — the KB is the unit of sharing.

use super::*;

/// One knowledge base.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct KbRow {
    pub name: String,
    pub display_name: String,
    pub owner_username: String,
    pub created_ms: i64,
}

/// One document of a KB, without its body.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct KbDocInfo {
    pub kb_name: String,
    pub path: String,
    pub title: String,
    pub mtime_ms: i64,
    pub sha256: String,
    pub source: String,
    pub bytes: u64,
}

/// One retrieval hit: the KB and path for the citation, the title, and the
/// FTS snippet around the match.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct KbHit {
    pub kb_name: String,
    pub path: String,
    pub title: String,
    pub snippet: String,
}

impl KbHit {
    /// The citation form the model sees and `turn_sources` records.
    pub fn citation(&self) -> String {
        format!("kb:{}/{}", self.kb_name, self.path)
    }
}

fn row_to_kb(r: &rusqlite::Row<'_>) -> rusqlite::Result<KbRow> {
    Ok(KbRow {
        name: r.get(0)?,
        display_name: r.get(1)?,
        owner_username: r.get(2)?,
        created_ms: r.get(3)?,
    })
}

impl Store {
    /// Create a KB owned by `owner` (no-op when it exists: `false`).
    pub fn create_kb(&self, name: &str, display_name: &str, owner: &str) -> Result<bool> {
        let conn = self.lock();
        let changed = conn.execute(
            "INSERT OR IGNORE INTO kbs(name, display_name, owner_username, created_ms)
             VALUES (?1, ?2, ?3, ?4)",
            params![name, display_name, owner, ka_agent_core::now_ms()],
        )?;
        Ok(changed == 1)
    }

    pub fn get_kb(&self, name: &str) -> Result<Option<KbRow>> {
        let conn = self.lock();
        conn.query_row(
            "SELECT name, display_name, owner_username, created_ms FROM kbs WHERE name = ?1",
            params![name],
            row_to_kb,
        )
        .optional()
        .map_err(StoreError::from)
    }

    pub fn list_kbs(&self) -> Result<Vec<KbRow>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT name, display_name, owner_username, created_ms FROM kbs ORDER BY name",
        )?;
        let rows = stmt.query_map([], row_to_kb)?;
        collect_rows(rows)
    }

    /// Delete a KB with its documents, FTS rows and access grants.
    pub fn delete_kb(&self, name: &str) -> Result<bool> {
        let mut conn = self.lock();
        let tx = conn.transaction()?;
        tx.execute("DELETE FROM kb_docs_fts WHERE kb_name = ?1", params![name])?;
        tx.execute("DELETE FROM kb_docs WHERE kb_name = ?1", params![name])?;
        tx.execute("DELETE FROM kb_access WHERE kb_name = ?1", params![name])?;
        let changed = tx.execute("DELETE FROM kbs WHERE name = ?1", params![name])?;
        tx.commit()?;
        Ok(changed == 1)
    }

    /// Replace a KB's access groups.
    pub fn set_kb_access(&self, kb: &str, groups: &[String]) -> Result<()> {
        let mut conn = self.lock();
        let tx = conn.transaction()?;
        tx.execute("DELETE FROM kb_access WHERE kb_name = ?1", params![kb])?;
        for group in groups {
            tx.execute(
                "INSERT OR IGNORE INTO kb_access(kb_name, group_name) VALUES (?1, ?2)",
                params![kb, group],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn kb_access(&self, kb: &str) -> Result<Vec<String>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT group_name FROM kb_access WHERE kb_name = ?1 ORDER BY group_name",
        )?;
        let rows = stmt.query_map(params![kb], |r| r.get::<_, String>(0))?;
        collect_rows(rows)
    }

    /// Every KB the user may read, alphabetical: their groups' KBs, the
    /// ones they own, and (role `admin`) all of them.
    pub fn kb_effective(&self, username: &str, role: &str) -> Result<Vec<String>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT name FROM kbs
             WHERE ?2 = 'admin'
                OR owner_username = ?1
                OR name IN (
                    SELECT kb_name FROM kb_access
                    WHERE group_name IN (SELECT group_name FROM users_groups WHERE username = ?1)
                )
             ORDER BY name",
        )?;
        let rows = stmt.query_map(
            params![username, role],
            |r| r.get::<_, String>(0),
        )?;
        collect_rows(rows)
    }

    /// Whether one KB is readable by the user (used by `@kb:` expansion and
    /// the web route, which name a KB before searching).
    pub fn kb_can_read(&self, kb: &str, username: &str, role: &str) -> Result<bool> {
        if role == ka_agent_core::ADMIN_ROLE {
            return Ok(self.get_kb(kb)?.is_some());
        }
        let conn = self.lock();
        let n: i64 = conn.query_row(
            "SELECT COUNT(*) FROM kbs
             WHERE name = ?1
               AND (owner_username = ?2
                    OR name IN (
                        SELECT kb_name FROM kb_access
                        WHERE group_name IN (SELECT group_name FROM users_groups WHERE username = ?2)
                    ))",
            params![kb, username],
            |r| r.get(0),
        )?;
        Ok(n > 0)
    }

    pub fn kb_doc_count(&self, kb: &str) -> Result<usize> {
        let conn = self.lock();
        let n: i64 = conn.query_row(
            "SELECT COUNT(*) FROM kb_docs WHERE kb_name = ?1",
            params![kb],
            |r| r.get(0),
        )?;
        Ok(n.max(0) as usize)
    }

    pub fn kb_list_docs(&self, kb: &str) -> Result<Vec<KbDocInfo>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT kb_name, path, title, mtime_ms, sha256, source, length(content)
             FROM kb_docs WHERE kb_name = ?1 ORDER BY path",
        )?;
        let rows = stmt.query_map(params![kb], |r| {
            Ok(KbDocInfo {
                kb_name: r.get(0)?,
                path: r.get(1)?,
                title: r.get(2)?,
                mtime_ms: r.get(3)?,
                sha256: r.get(4)?,
                source: r.get(5)?,
                bytes: r.get::<_, i64>(6)?.max(0) as u64,
            })
        })?;
        collect_rows(rows)
    }

    /// Insert or replace one document (same `path` replaces), keeping the
    /// FTS mirror in step. Returns `true` when a new document was created.
    #[allow(clippy::too_many_arguments)]
    pub fn kb_upsert_doc(
        &self,
        kb: &str,
        path: &str,
        title: &str,
        content: &str,
        sha256: &str,
        source: &str,
        mtime_ms: i64,
    ) -> Result<bool> {
        let mut conn = self.lock();
        let tx = conn.transaction()?;
        let existed: Option<i64> = tx
            .query_row(
                "SELECT doc_id FROM kb_docs WHERE kb_name = ?1 AND path = ?2",
                params![kb, path],
                |r| r.get(0),
            )
            .optional()?;
        tx.execute("DELETE FROM kb_docs_fts WHERE kb_name = ?1 AND path = ?2", params![kb, path])?;
        tx.execute(
            "INSERT INTO kb_docs(kb_name, path, title, content, mtime_ms, sha256, source)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT(kb_name, path) DO UPDATE SET
                title = ?3, content = ?4, mtime_ms = ?5, sha256 = ?6, source = ?7",
            params![kb, path, title, content, mtime_ms, sha256, source],
        )?;
        tx.execute(
            "INSERT INTO kb_docs_fts(kb_name, path, title, content) VALUES (?1, ?2, ?3, ?4)",
            params![kb, path, title, content],
        )?;
        tx.commit()?;
        Ok(existed.is_none())
    }

    /// Remove one document; `true` when it existed.
    pub fn kb_remove_doc(&self, kb: &str, path: &str) -> Result<bool> {
        let mut conn = self.lock();
        let tx = conn.transaction()?;
        tx.execute("DELETE FROM kb_docs_fts WHERE kb_name = ?1 AND path = ?2", params![kb, path])?;
        let changed = tx.execute(
            "DELETE FROM kb_docs WHERE kb_name = ?1 AND path = ?2",
            params![kb, path],
        )?;
        tx.commit()?;
        Ok(changed == 1)
    }

    /// One document's `(title, content)`, for `@kb:<name>/<path>` expansion.
    pub fn kb_get_doc(&self, kb: &str, path: &str) -> Result<Option<(String, String)>> {
        let conn = self.lock();
        conn.query_row(
            "SELECT title, content FROM kb_docs WHERE kb_name = ?1 AND path = ?2",
            params![kb, path],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()
        .map_err(StoreError::from)
    }

    /// FTS search over the readable KBs (`kbs` is the caller's effective
    /// set, so access is decided before the query runs).
    pub fn kb_search(&self, kbs: &[String], query: &str, limit: usize) -> Result<Vec<KbHit>> {
        if kbs.is_empty() {
            return Ok(Vec::new());
        }
        let match_expr = crate::turns::fts_and_query(query);
        if match_expr.is_empty() {
            return Ok(Vec::new());
        }
        let placeholders = (0..kbs.len())
            .map(|i| format!("?{}", i + 2))
            .collect::<Vec<_>>()
            .join(",");
        let sql = format!(
            "SELECT kb_name, path, title, snippet(kb_docs_fts, 3, '[', ']', '…', 48)
             FROM kb_docs_fts
             WHERE kb_docs_fts MATCH ?1 AND kb_name IN ({placeholders})
             ORDER BY rank LIMIT {}",
            limit.clamp(1, 50)
        );
        let conn = self.lock();
        let mut stmt = conn.prepare(&sql)?;
        let mut params_vec: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
        params_vec.push(Box::new(match_expr));
        for kb in kbs {
            params_vec.push(Box::new(kb.clone()));
        }
        let refs: Vec<&dyn rusqlite::ToSql> = params_vec.iter().map(|b| b.as_ref()).collect();
        let rows = stmt.query_map(refs.as_slice(), |r| {
            Ok(KbHit {
                kb_name: r.get(0)?,
                path: r.get(1)?,
                title: r.get(2)?,
                snippet: r.get(3)?,
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
    fn kb_access_follows_groups_owner_and_admin() {
        let store = Store::open_in_memory().unwrap();
        // The v12 migration creates `operator`, and a KB owner must exist.
        assert!(store.get_user("operator").unwrap().is_some());
        store.upsert_group("ops", &[], None).unwrap();
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
        store.create_kb("handbook", "Handbook", "operator").unwrap();
        store.create_kb("secret", "Secret", "operator").unwrap();
        assert!(!store.create_kb("handbook", "Handbook", "operator").unwrap());
        store
            .set_kb_access("handbook", &["ops".to_string()])
            .unwrap();
        assert_eq!(store.kb_access("handbook").unwrap(), vec!["ops"]);

        // A member of ops reads the handbook only.
        store.set_user_groups("ana", &["ops".into()]).unwrap();
        assert_eq!(store.kb_effective("ana", "member").unwrap(), vec!["handbook"]);
        assert!(store.kb_can_read("handbook", "ana", "member").unwrap());
        assert!(!store.kb_can_read("secret", "ana", "member").unwrap());
        assert!(!store.kb_can_read("missing", "ana", "member").unwrap());
        // A non-member reads nothing.
        store.set_user_groups("bob", &[]).unwrap();
        assert!(store.kb_effective("bob", "member").unwrap().is_empty());
        // Admin reads everything.
        assert_eq!(
            store.kb_effective("operator", "admin").unwrap(),
            vec!["handbook", "secret"]
        );
        // Access grants do not resurrect a deleted KB.
        assert!(store.delete_kb("secret").unwrap());
        assert!(store.get_kb("secret").unwrap().is_none());
        assert_eq!(store.list_kbs().unwrap().len(), 1);
    }

    #[test]
    fn documents_replace_in_place_and_are_searchable() {
        let store = Store::open_in_memory().unwrap();
        store.create_kb("docs", "Docs", "operator").unwrap();
        assert!(
            store
                .kb_upsert_doc("docs", "a.md", "Alpha", "the quick brown fox", "h1", "upload", 1)
                .unwrap()
        );
        // Same path replaces: not a second document, FTS not duplicated.
        assert!(
            !store
                .kb_upsert_doc("docs", "a.md", "Alpha", "a slow green turtle", "h2", "upload", 2)
                .unwrap()
        );
        assert_eq!(store.kb_doc_count("docs").unwrap(), 1);
        let docs = store.kb_list_docs("docs").unwrap();
        assert_eq!(docs[0].sha256, "h2");
        assert!(docs[0].bytes > 0);

        let hits = store
            .kb_search(&["docs".to_string()], "green turtle", 8)
            .unwrap();
        assert_eq!(hits.len(), 1, "{hits:?}");
        assert_eq!(hits[0].kb_name, "docs");
        assert_eq!(hits[0].path, "a.md");
        assert!(hits[0].snippet.contains("green"), "{}", hits[0].snippet);
        assert_eq!(hits[0].citation(), "kb:docs/a.md");
        // The replaced content is gone from the index.
        assert!(
            store
                .kb_search(&["docs".to_string()], "brown fox", 8)
                .unwrap()
                .is_empty()
        );
        // Searching a KB the caller cannot read finds nothing.
        assert!(
            store
                .kb_search(&["secret".to_string()], "green turtle", 8)
                .unwrap()
                .is_empty()
        );

        assert_eq!(
            store.kb_get_doc("docs", "a.md").unwrap(),
            Some(("Alpha".into(), "a slow green turtle".into()))
        );
        assert!(store.kb_remove_doc("docs", "a.md").unwrap());
        assert!(!store.kb_remove_doc("docs", "a.md").unwrap());
        assert!(store.kb_search(&["docs".to_string()], "green", 8).unwrap().is_empty());
    }
}
