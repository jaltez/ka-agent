//! Accepted-memory FTS mirror (A, phase 0).
//!
//! The Markdown files remain the source of truth. This module stores one
//! derived FTS row per accepted bullet so `/memory search` and
//! `kagent__memory_search` can rank memory without reading every file.

use super::*;

/// One accepted memory bullet as the FTS mirror stores it.
#[derive(Debug, Clone)]
pub struct MemoryIndexRow {
    pub id: String,
    pub file: String,
    pub text: String,
    pub source: String,
    pub keywords: String,
    pub accepted_ms: i64,
    pub valid_until: Option<String>,
}

/// One hit from [`Store::search_memory_fts_for_user`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryHit {
    pub id: String,
    pub file: String,
    pub text: String,
    pub source: String,
    pub snippet: String,
    pub accepted_ms: i64,
    pub valid_until: Option<String>,
}

impl Store {
    /// Replace every FTS row for one user in one transaction. The caller
    /// rebuilds the rows from `MemoryStore::entries`, so a stale row can
    /// never survive a mutation.
    pub fn replace_memory_fts_for_user(
        &self,
        username: &str,
        rows: &[MemoryIndexRow],
    ) -> Result<()> {
        let mut conn = self.lock();
        let tx = conn.transaction()?;
        tx.execute(
            "DELETE FROM memory_fts WHERE username = ?1",
            params![username],
        )?;
        {
            let mut stmt = tx.prepare(
                "INSERT INTO memory_fts
                 (id, username, file, text, keywords, source, accepted_ms, valid_until)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            )?;
            for row in rows {
                stmt.execute(params![
                    &row.id,
                    username,
                    &row.file,
                    &row.text,
                    &row.keywords,
                    &row.source,
                    row.accepted_ms,
                    row.valid_until.as_deref(),
                ])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// FTS search over one user's accepted memory. The raw query goes
    /// through the same safe AND-of-quoted-terms tokenizer as `/history`,
    /// so nothing is ever interpreted as FTS syntax.
    pub fn search_memory_fts_for_user(
        &self,
        username: &str,
        query: &str,
        limit: usize,
    ) -> Result<Vec<MemoryHit>> {
        let match_expr = crate::turns::fts_or_query(query);
        if match_expr.is_empty() {
            return Ok(Vec::new());
        }
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT id, file, text, source,
                    snippet(memory_fts, 3, '[', ']', '…', 12),
                    accepted_ms, valid_until
             FROM memory_fts
             WHERE memory_fts MATCH ?1 AND username = ?2
             ORDER BY rank LIMIT ?3",
        )?;
        let rows = stmt.query_map(params![match_expr, username, limit as i64], |r| {
            Ok(MemoryHit {
                id: r.get(0)?,
                file: r.get(1)?,
                text: r.get(2)?,
                source: r.get(3)?,
                snippet: r.get(4)?,
                accepted_ms: r.get(5)?,
                valid_until: r.get(6)?,
            })
        })?;
        collect_rows(rows)
    }

    /// Row count for one user, used to detect a missing or stale mirror.
    pub fn count_memory_fts_for_user(&self, username: &str) -> Result<usize> {
        let conn = self.lock();
        let count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM memory_fts WHERE username = ?1",
            params![username],
            |r| r.get(0),
        )?;
        Ok(count.max(0) as usize)
    }

    /// Drop every derived memory row for a purged user. The Markdown tree
    /// is deleted by `ka-agent user purge`; the derived text must not
    /// outlive it.
    pub fn delete_memory_fts_for_user(&self, username: &str) -> Result<()> {
        let conn = self.lock();
        conn.execute(
            "DELETE FROM memory_fts WHERE username = ?1",
            params![username],
        )?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    fn row(id: &str, text: &str) -> MemoryIndexRow {
        MemoryIndexRow {
            id: id.into(),
            file: "facts.md".into(),
            text: text.into(),
            source: "operator".into(),
            keywords: String::new(),
            accepted_ms: 1_700_000_000_000,
            valid_until: None,
        }
    }

    #[test]
    fn search_finds_a_bullet_and_isolates_users() {
        let store = Store::open_in_memory().expect("store");
        store
            .replace_memory_fts_for_user(
                "alice",
                &[
                    row("mem_a", "prefers concise Spanish answers"),
                    row("mem_b", "deploys from the staging branch"),
                ],
            )
            .expect("index");
        store
            .replace_memory_fts_for_user("bob", &[row("mem_c", "prefers concise English answers")])
            .expect("index bob");

        let hits = store
            .search_memory_fts_for_user("alice", "Spanish answers", 5)
            .expect("search");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].id, "mem_a");
        assert!(hits[0].snippet.contains("Spanish"));
        assert_eq!(
            store
                .count_memory_fts_for_user("alice")
                .expect("count alice"),
            2
        );
        assert_eq!(
            store
                .search_memory_fts_for_user("bob", "Spanish", 5)
                .expect("bob search")
                .len(),
            0
        );
        assert_eq!(
            store
                .search_memory_fts_for_user("bob", "English", 5)
                .expect("bob search")
                .len(),
            1
        );
        assert!(
            !store
                .search_memory_fts_for_user("alice", "concise", 5)
                .expect("alice search")
                .is_empty()
        );
    }

    #[test]
    fn keywords_are_searchable_too() {
        let store = Store::open_in_memory().expect("store");
        let mut with_keywords = row("mem_k", "likes short replies");
        with_keywords.keywords = "concise telegram".into();
        store
            .replace_memory_fts_for_user("alice", &[with_keywords])
            .expect("index");
        let hits = store
            .search_memory_fts_for_user("alice", "telegram", 5)
            .expect("search");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].id, "mem_k");
    }

    #[test]
    fn purge_deletes_only_that_users_rows() {
        let store = Store::open_in_memory().expect("store");
        store
            .replace_memory_fts_for_user("alice", &[row("mem_a", "alice fact")])
            .expect("alice");
        store
            .replace_memory_fts_for_user("bob", &[row("mem_b", "bob fact")])
            .expect("bob");
        store.delete_memory_fts_for_user("alice").expect("purge");
        assert_eq!(store.count_memory_fts_for_user("alice").expect("count"), 0);
        assert_eq!(store.count_memory_fts_for_user("bob").expect("count"), 1);
    }

    #[test]
    fn replace_removes_the_old_rows_for_that_user_only() {
        let store = Store::open_in_memory().expect("store");
        store
            .replace_memory_fts_for_user("alice", &[row("mem_a", "old fact")])
            .expect("first");
        store
            .replace_memory_fts_for_user("bob", &[row("mem_b", "bob fact")])
            .expect("bob");
        store
            .replace_memory_fts_for_user("alice", &[row("mem_c", "new fact")])
            .expect("second");
        let alice = store
            .search_memory_fts_for_user("alice", "new", 5)
            .expect("search");
        assert_eq!(alice.len(), 1);
        assert_eq!(alice[0].id, "mem_c");
        assert!(
            store
                .search_memory_fts_for_user("alice", "old", 5)
                .expect("old")
                .is_empty()
        );
        assert_eq!(
            store
                .search_memory_fts_for_user("bob", "bob", 5)
                .expect("bob")
                .len(),
            1
        );
    }
}
