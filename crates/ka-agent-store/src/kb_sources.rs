//! Knowledge sources (M3): the per-KB sync configuration and resume cursor.

use super::*;

/// One configured external source feeding a knowledge base.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct KbSourceRow {
    pub kb_name: String,
    /// `notion` | `dropbox` | `git`.
    pub kind: String,
    /// Root page id, folder path or repository URL.
    pub target: String,
    /// Secret-store name for the source token (notion/dropbox).
    pub token_secret: Option<String>,
    /// File globs to include (`**/*.md`); empty = every supported file.
    pub include_globs: Vec<String>,
    /// The connector's resume token.
    pub cursor: Option<String>,
    pub schedule_ms: i64,
    pub created_ms: i64,
}

fn row_to_source(r: &rusqlite::Row<'_>) -> rusqlite::Result<KbSourceRow> {
    let globs: Option<String> = r.get(4)?;
    Ok(KbSourceRow {
        kb_name: r.get(0)?,
        kind: r.get(1)?,
        target: r.get(2)?,
        token_secret: r.get(3)?,
        include_globs: globs
            .and_then(|g| serde_json::from_str(&g).ok())
            .unwrap_or_default(),
        cursor: r.get(5)?,
        schedule_ms: r.get(6)?,
        created_ms: r.get(7)?,
    })
}

const SOURCE_COLS: &str =
    "kb_name, kind, target, token_secret, include_globs, cursor, schedule_ms, created_ms";

impl Store {
    pub fn put_kb_source(&self, src: &KbSourceRow) -> Result<()> {
        if !matches!(src.kind.as_str(), "notion" | "dropbox" | "git") {
            return Err(StoreError::Refused(format!(
                "knowledge source kind {:?} must be notion, dropbox or git",
                src.kind
            )));
        }
        if self.get_kb(&src.kb_name)?.is_none() {
            return Err(StoreError::NotFound(format!("kb {}", src.kb_name)));
        }
        let globs = if src.include_globs.is_empty() {
            None
        } else {
            Some(serde_json::to_string(&src.include_globs).unwrap_or_else(|_| "[]".into()))
        };
        let conn = self.lock();
        conn.execute(
            "INSERT INTO kb_sources(kb_name, kind, target, token_secret, include_globs, cursor, schedule_ms, created_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
             ON CONFLICT(kb_name, kind, target) DO UPDATE SET
                token_secret = ?4, include_globs = ?5, schedule_ms = ?7",
            params![
                src.kb_name,
                src.kind,
                src.target,
                src.token_secret,
                globs,
                src.cursor,
                src.schedule_ms,
                src.created_ms
            ],
        )?;
        Ok(())
    }

    pub fn list_kb_sources(&self) -> Result<Vec<KbSourceRow>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(&format!(
            "SELECT {SOURCE_COLS} FROM kb_sources ORDER BY kb_name, kind, target"
        ))?;
        let rows = stmt.query_map([], row_to_source)?;
        collect_rows(rows)
    }

    pub fn kb_sources_for(&self, kb: &str) -> Result<Vec<KbSourceRow>> {
        Ok(self
            .list_kb_sources()?
            .into_iter()
            .filter(|s| s.kb_name == kb)
            .collect())
    }

    pub fn delete_kb_source(&self, kb: &str, kind: &str, target: &str) -> Result<bool> {
        let conn = self.lock();
        Ok(conn.execute(
            "DELETE FROM kb_sources WHERE kb_name = ?1 AND kind = ?2 AND target = ?3",
            params![kb, kind, target],
        )? == 1)
    }

    /// Persist a connector's resume cursor.
    pub fn set_kb_source_cursor(&self, kb: &str, kind: &str, target: &str, cursor: &str) -> Result<()> {
        let conn = self.lock();
        conn.execute(
            "UPDATE kb_sources SET cursor = ?4 WHERE kb_name = ?1 AND kind = ?2 AND target = ?3",
            params![kb, kind, target, cursor],
        )?;
        Ok(())
    }

    /// Remove every document a source indexed but the source no longer has
    /// (the connector passes the paths it saw). Returns how many went away.
    pub fn kb_prune_absent(&self, kb: &str, seen: &[String]) -> Result<usize> {
        let mut removed = 0;
        for doc in self.kb_list_docs(kb)? {
            if !seen.contains(&doc.path)
                && self.kb_remove_doc(kb, &doc.path)? {
                    removed += 1;
                }
        }
        Ok(removed)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[test]
    fn sources_roundtrip_and_prune() {
        let store = Store::open_in_memory().unwrap();
        store
            .create_kb("docs", "Docs", ka_agent_core::OPERATOR_USER)
            .unwrap();
        let src = KbSourceRow {
            kb_name: "docs".into(),
            kind: "dropbox".into(),
            target: "/team".into(),
            token_secret: Some("dropbox.token".into()),
            include_globs: vec!["**/*.md".into()],
            cursor: None,
            schedule_ms: 900_000,
            created_ms: 1,
        };
        store.put_kb_source(&src).unwrap();
        assert_eq!(store.kb_sources_for("docs").unwrap(), vec![src.clone()]);
        store
            .set_kb_source_cursor("docs", "dropbox", "/team", "cur-1")
            .unwrap();
        assert_eq!(
            store.kb_sources_for("docs").unwrap()[0].cursor.as_deref(),
            Some("cur-1")
        );
        // An unknown KB or kind is refused.
        assert!(store
            .put_kb_source(&KbSourceRow {
                kb_name: "nope".into(),
                ..src.clone()
            })
            .is_err());
        assert!(store
            .put_kb_source(&KbSourceRow {
                kind: "s3".into(),
                ..src.clone()
            })
            .is_err());
        // Prune removes docs the source no longer reports.
        store
            .kb_upsert_doc("docs", "a.md", "A", "x", "h1", "dropbox", 1)
            .unwrap();
        store
            .kb_upsert_doc("docs", "b.md", "B", "y", "h2", "dropbox", 1)
            .unwrap();
        assert_eq!(
            store
                .kb_prune_absent("docs", &["a.md".to_string()])
                .unwrap(),
            1
        );
        assert!(store.kb_get_doc("docs", "b.md").unwrap().is_none());
        assert!(store.delete_kb_source("docs", "dropbox", "/team").unwrap());
    }
}
