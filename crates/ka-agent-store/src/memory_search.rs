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

/// The validity date a memory search runs at. `ms` bounds `accepted_ms`
/// (a memory accepted after the as-of date was not knowable then); `date`
/// is the `YYYY-MM-DD` compared with `valid_until`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryAsOf {
    pub ms: i64,
    pub date: String,
}

impl MemoryAsOf {
    pub fn new(ms: i64, date: impl Into<String>) -> Self {
        Self {
            ms,
            date: date.into(),
        }
    }
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

fn insert_memory_fts_rows(
    tx: &rusqlite::Transaction<'_>,
    username: &str,
    rows: &[MemoryIndexRow],
) -> Result<()> {
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
    Ok(())
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
        insert_memory_fts_rows(&tx, username, rows)?;
        tx.commit()?;
        Ok(())
    }

    /// Replace the FTS rows and drop the vectors in one transaction. The
    /// memory-change refresh path uses this, so a forgotten bullet's
    /// embedding cannot outlive its FTS row.
    pub fn replace_memory_derived_for_user(
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
        insert_memory_fts_rows(&tx, username, rows)?;
        tx.execute(
            "DELETE FROM memory_vectors WHERE username = ?1",
            params![username],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Drop both derived tables for a purged user in one transaction.
    pub fn delete_memory_derived_for_user(&self, username: &str) -> Result<()> {
        let mut conn = self.lock();
        let tx = conn.transaction()?;
        tx.execute(
            "DELETE FROM memory_fts WHERE username = ?1",
            params![username],
        )?;
        tx.execute(
            "DELETE FROM memory_vectors WHERE username = ?1",
            params![username],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// FTS search over one user's accepted memory as of `as_of`. The raw
    /// query goes through the same safe AND-of-quoted-terms tokenizer as
    /// `/history`, so nothing is ever interpreted as FTS syntax. Expired
    /// bullets and bullets accepted after `as_of` are excluded; pass the
    /// as-of date to read history, today to read the current view.
    pub fn search_memory_fts_for_user(
        &self,
        username: &str,
        query: &str,
        limit: usize,
        as_of: &MemoryAsOf,
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
               AND accepted_ms <= ?3
               AND (valid_until IS NULL OR valid_until = '' OR valid_until >= ?4)
             ORDER BY rank LIMIT ?5",
        )?;
        let rows = stmt.query_map(
            params![match_expr, username, as_of.ms, as_of.date, limit as i64],
            |r| {
                Ok(MemoryHit {
                    id: r.get(0)?,
                    file: r.get(1)?,
                    text: r.get(2)?,
                    source: r.get(3)?,
                    snippet: r.get(4)?,
                    accepted_ms: r.get(5)?,
                    valid_until: r.get(6)?,
                })
            },
        )?;
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

    fn as_of(date: &str) -> MemoryAsOf {
        MemoryAsOf::new(1_800_000_000_000, date)
    }

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
            .search_memory_fts_for_user("alice", "Spanish answers", 5, &as_of("2026-11-26"))
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
                .search_memory_fts_for_user("bob", "Spanish", 5, &as_of("2026-11-26"))
                .expect("bob search")
                .len(),
            0
        );
        assert_eq!(
            store
                .search_memory_fts_for_user("bob", "English", 5, &as_of("2026-11-26"))
                .expect("bob search")
                .len(),
            1
        );
        assert!(
            !store
                .search_memory_fts_for_user("alice", "concise", 5, &as_of("2026-11-26"))
                .expect("alice search")
                .is_empty()
        );
    }

    #[test]
    fn search_filters_expired_and_honors_as_of() {
        let store = Store::open_in_memory().expect("store");
        let mut old = row("mem_old", "the staging URL is staging-old");
        old.valid_until = Some("2029-01-01".into());
        let mut new = row("mem_new", "the staging URL is staging-new");
        new.valid_until = Some("2031-01-01".into());
        store
            .replace_memory_fts_for_user("alice", &[old, new])
            .expect("index");

        // Before the old entry expires, both are valid.
        let both = store
            .search_memory_fts_for_user("alice", "staging", 10, &as_of("2028-01-01"))
            .expect("as-of 2028");
        assert_eq!(both.len(), 2, "{both:?}");

        // After the old entry expires, only the current one is returned.
        let current = store
            .search_memory_fts_for_user("alice", "staging", 10, &as_of("2030-01-01"))
            .expect("as-of 2030");
        assert_eq!(current.len(), 1, "{current:?}");
        assert_eq!(current[0].id, "mem_new");

        // Before the entries were accepted, nothing was knowable.
        assert!(
            store
                .search_memory_fts_for_user(
                    "alice",
                    "staging",
                    10,
                    &MemoryAsOf::new(1_577_836_800_000, "2020-01-01"),
                )
                .expect("as-of 2020")
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
            .search_memory_fts_for_user("alice", "telegram", 5, &as_of("2026-11-26"))
            .expect("search");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].id, "mem_k");
    }

    #[test]
    fn derived_replace_clears_vectors_in_one_call() {
        let store = Store::open_in_memory().unwrap();
        store
            .replace_memory_vectors_for_user(
                "alice",
                "test-model",
                2,
                &[crate::MemoryVectorRow {
                    ref_id: "mem_old".into(),
                    scope: None,
                    vec: vec![1.0, 0.0],
                    updated_ms: 1,
                }],
            )
            .unwrap();
        store
            .replace_memory_derived_for_user("alice", &[row("mem_new", "new fact")])
            .unwrap();
        assert_eq!(store.count_memory_vectors_for_user("alice").unwrap(), 0);
        let hits = store
            .search_memory_fts_for_user("alice", "new", 5, &as_of("2026-11-26"))
            .unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].id, "mem_new");
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
            .search_memory_fts_for_user("alice", "new", 5, &as_of("2026-11-26"))
            .expect("search");
        assert_eq!(alice.len(), 1);
        assert_eq!(alice[0].id, "mem_c");
        assert!(
            store
                .search_memory_fts_for_user("alice", "old", 5, &as_of("2026-11-26"))
                .expect("old")
                .is_empty()
        );
        assert_eq!(
            store
                .search_memory_fts_for_user("bob", "bob", 5, &as_of("2026-11-26"))
                .expect("bob")
                .len(),
            1
        );
    }
}
