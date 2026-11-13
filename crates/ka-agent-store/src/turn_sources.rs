//! Provenance: which KBs, files and URLs fed a turn (M3 share-time check).
//!
//! The runtime records one row per source when a turn retrieves from a KB or
//! expands a `@file`/URL reference. Before a derived result is delivered to
//! another user or a shared channel, the recipient's groups are intersected
//! with each source row's `group_names`: a miss blocks the delivery.

use super::*;

/// One source a turn consumed.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TurnSourceRow {
    pub turn_id: String,
    /// `kb` | `file` | `url`.
    pub kind: String,
    /// `kb:<name>/<path>`, a workspace-relative path, or a URL.
    pub reference: String,
    /// The groups allowed to see this source; empty = everyone the turn's
    /// output goes to (an operator-owned local file).
    pub group_names: Vec<String>,
}

/// One reference staged for the turn that is about to be submitted (the
/// expansion happens before the turn row exists).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StagedSource {
    /// `kb` | `file` | `url`.
    pub kind: String,
    /// The citation form (`kb:docs/a.md`, a workspace path, a URL).
    pub reference: String,
    /// The groups that may see it (empty = no restriction recorded).
    pub groups: Vec<String>,
}

impl Store {
    /// Stage the sources of one inbound message; the runtime moves them into
    /// `turn_sources` as soon as the turn row exists. The slot is a FIFO of
    /// batches: a second message sent while a turn is running must not have
    /// its sources claimed by the earlier turn.
    pub fn stage_turn_sources(&self, source: &str, sources: &[StagedSource]) {
        if sources.is_empty() {
            return;
        }
        let key = format!("refs:{source}");
        let mut batches: Vec<Vec<StagedSource>> = self
            .meta_get(&key)
            .ok()
            .flatten()
            .and_then(|raw| serde_json::from_str(&raw).ok())
            .unwrap_or_default();
        batches.push(sources.to_vec());
        if let Ok(payload) = serde_json::to_string(&batches) {
            let _ = self.meta_set(&key, &payload);
        }
    }

    /// Pop the oldest staged batch of one source into `turn_sources`; returns
    /// how many rows were recorded.
    pub fn record_staged_sources(&self, source: &str, turn_id: &str) -> usize {
        let key = format!("refs:{source}");
        let Some(raw) = self.meta_get(&key).ok().flatten() else {
            return 0;
        };
        if raw.is_empty() {
            return 0;
        }
        let mut batches: Vec<Vec<StagedSource>> =
            serde_json::from_str(&raw).unwrap_or_default();
        let Some(sources) = (!batches.is_empty()).then(|| batches.remove(0)) else {
            return 0;
        };
        if batches.is_empty() {
            let _ = self.meta_set(&key, "");
        } else if let Ok(payload) = serde_json::to_string(&batches) {
            let _ = self.meta_set(&key, &payload);
        }
        let mut n = 0;
        for s in sources {
            if self
                .record_turn_source(turn_id, &s.kind, &s.reference, &s.groups)
                .is_ok()
            {
                n += 1;
            }
        }
        n
    }

    /// Record one source of one turn (idempotent per `(turn, kind, ref)`).
    pub fn record_turn_source(
        &self,
        turn: &str,
        kind: &str,
        reference: &str,
        groups: &[String],
    ) -> Result<()> {
        let groups = if groups.is_empty() {
            None
        } else {
            Some(serde_json::to_string(groups).unwrap_or_else(|_| "[]".into()))
        };
        let conn = self.lock();
        conn.execute(
            "INSERT OR REPLACE INTO turn_sources(turn_id, kind, ref, group_names)
             VALUES (?1, ?2, ?3, ?4)",
            params![turn, kind, reference, groups],
        )?;
        Ok(())
    }

    /// Every source recorded for one turn.
    pub fn turn_sources(&self, turn: &str) -> Result<Vec<TurnSourceRow>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT turn_id, kind, ref, group_names FROM turn_sources
             WHERE turn_id = ?1 ORDER BY kind, ref",
        )?;
        let rows = stmt.query_map(params![turn], |r| {
            let groups: Option<String> = r.get(3)?;
            Ok(TurnSourceRow {
                turn_id: r.get(0)?,
                kind: r.get(1)?,
                reference: r.get(2)?,
                group_names: groups
                    .and_then(|g| serde_json::from_str::<Vec<String>>(&g).ok())
                    .unwrap_or_default(),
            })
        })?;
        collect_rows(rows)
    }

    /// Drop a turn's provenance (session purge).
    pub fn delete_turn_sources(&self, turn: &str) -> Result<usize> {
        let conn = self.lock();
        Ok(conn.execute("DELETE FROM turn_sources WHERE turn_id = ?1", params![turn])?)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[test]
    fn staged_batches_queue_per_source() {
        let store = Store::open_in_memory().unwrap();
        store.stage_turn_sources(
            "telegram:1",
            &[StagedSource {
                kind: "kb".into(),
                reference: "kb:a/1.md".into(),
                groups: vec!["ops".into()],
            }],
        );
        // A second message stages another batch before the first turn exists.
        store.stage_turn_sources(
            "telegram:1",
            &[StagedSource {
                kind: "kb".into(),
                reference: "kb:a/2.md".into(),
                groups: Vec::new(),
            }],
        );
        // The first turn records the FIRST batch, the second turn the second.
        assert_eq!(store.record_staged_sources("telegram:1", "turn_1"), 1);
        assert_eq!(
            store.turn_sources("turn_1").unwrap()[0].reference,
            "kb:a/1.md"
        );
        assert_eq!(store.record_staged_sources("telegram:1", "turn_2"), 1);
        assert_eq!(
            store.turn_sources("turn_2").unwrap()[0].reference,
            "kb:a/2.md"
        );
        // Drained: a later turn records nothing.
        assert_eq!(store.record_staged_sources("telegram:1", "turn_3"), 0);
    }

    #[test]
    fn sources_roundtrip_and_replace() {
        let store = Store::open_in_memory().unwrap();
        store
            .record_turn_source("turn_1", "kb", "kb:docs/a.md", &["dev".into()])
            .unwrap();
        store
            .record_turn_source("turn_1", "file", "notes/plan.md", &[])
            .unwrap();
        // Same source recorded twice keeps one row.
        store
            .record_turn_source("turn_1", "kb", "kb:docs/a.md", &["dev".into(), "ops".into()])
            .unwrap();
        let rows = store.turn_sources("turn_1").unwrap();
        assert_eq!(rows.len(), 2);
        let kb = rows.iter().find(|r| r.kind == "kb").unwrap();
        assert_eq!(kb.group_names, vec!["dev".to_string(), "ops".to_string()]);
        let file = rows.iter().find(|r| r.kind == "file").unwrap();
        assert!(file.group_names.is_empty());
        assert!(store.turn_sources("turn_2").unwrap().is_empty());
        assert_eq!(store.delete_turn_sources("turn_1").unwrap(), 2);
    }
}
