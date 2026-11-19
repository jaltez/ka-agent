//! Shadow-git checkpoint rows (B1): insert at the single Allow-settle
//! site, list for `/rewind`, prune with the retention pass. The git
//! repos themselves live under each user's `checkpoints/` partition and
//! follow these rows (a session whose rows are all gone loses its repo).

use super::*;

/// One checkpoint: the workspace snapshot taken just before an
/// Allow-settled write ran.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CheckpointRow {
    pub id: String,
    pub session: SessionId,
    pub turn: TurnId,
    pub username: String,
    /// The approval card the operator settled, when one existed —
    /// `None` for session-allow and rule settles.
    pub approval: Option<String>,
    /// The ask's question, replayed in the `/rewind <n>` reply.
    pub question: Option<String>,
    /// The snapshot commit; `None` when the snapshot failed (the gap).
    pub sha: Option<String>,
    pub created_ms: i64,
}

impl Store {
    /// Record one checkpoint. `INSERT OR IGNORE`: the id is
    /// `(turn, created_ms)` and a same-millisecond replay of the same
    /// settle is the same checkpoint.
    pub fn insert_checkpoint(&self, row: &CheckpointRow) -> Result<()> {
        let conn = self.lock();
        conn.execute(
            "INSERT OR IGNORE INTO checkpoints(id, session, turn, username, approval, question, sha, created_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                row.id,
                row.session.0,
                row.turn.0,
                row.username,
                row.approval,
                row.question,
                row.sha,
                row.created_ms
            ],
        )?;
        Ok(())
    }

    /// One session's checkpoints, newest first (`/rewind` numbering).
    pub fn checkpoints_for_session(&self, session: &SessionId) -> Result<Vec<CheckpointRow>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT id, session, turn, username, approval, question, sha, created_ms
             FROM checkpoints WHERE session = ?1 ORDER BY created_ms DESC, id DESC",
        )?;
        let rows = stmt.query_map(params![session.0], row_to_checkpoint)?;
        collect_rows(rows)
    }

    /// Delete checkpoint rows older than `keep_days` (0 = keep forever).
    /// Returns what went; the scheduler then drops repo directories
    /// whose sessions have no rows left.
    pub fn prune_checkpoints(&self, now_ms: i64, keep_days: u32) -> Result<usize> {
        if keep_days == 0 {
            return Ok(0);
        }
        let cutoff = now_ms - (keep_days as i64) * 86_400_000;
        let conn = self.lock();
        Ok(conn.execute("DELETE FROM checkpoints WHERE created_ms < ?1", [cutoff])?)
    }

    /// Every `(username, short session id)` that still has checkpoint
    /// rows — the keep-set for repo-directory pruning.
    pub fn checkpoint_dir_keys(&self) -> Result<Vec<String>> {
        let conn = self.lock();
        let mut stmt =
            conn.prepare("SELECT DISTINCT username, substr(session, -8) FROM checkpoints")?;
        let rows = stmt.query_map([], |r| {
            let user: String = r.get(0)?;
            let short: String = r.get(1)?;
            Ok(format!("{user}/{short}"))
        })?;
        collect_rows(rows)
    }
}

fn row_to_checkpoint(r: &rusqlite::Row<'_>) -> rusqlite::Result<CheckpointRow> {
    Ok(CheckpointRow {
        id: r.get(0)?,
        session: SessionId(r.get(1)?),
        turn: TurnId(r.get(2)?),
        username: r.get(3)?,
        approval: r.get(4)?,
        question: r.get(5)?,
        sha: r.get(6)?,
        created_ms: r.get(7)?,
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use crate::{CheckpointRow, Store, session};

    fn row(session: &crate::SessionId, tag: &str, created_ms: i64) -> CheckpointRow {
        CheckpointRow {
            id: format!("c-{tag}"),
            session: session.clone(),
            turn: ka_agent_core::TurnId(format!("turn-{tag}")),
            username: "operator".into(),
            approval: Some(format!("appr-{tag}")),
            question: Some(format!("run {tag}?")),
            sha: Some(format!("sha{tag}")),
            created_ms,
        }
    }

    #[test]
    fn lists_newest_first_and_prunes_by_window() {
        let store = Store::open_in_memory().unwrap();
        store.insert_session(&session()).unwrap();
        let sid = store
            .list_recent_sessions(1)
            .unwrap()
            .pop()
            .map(|s| s.id)
            .unwrap();
        let day = 86_400_000i64;
        let now = 1_800_000_000_000i64;
        store
            .insert_checkpoint(&row(&sid, "old", now - 40 * day))
            .unwrap();
        store
            .insert_checkpoint(&row(&sid, "new", now - day))
            .unwrap();
        let listed = store.checkpoints_for_session(&sid).unwrap();
        assert_eq!(listed.len(), 2);
        assert_eq!(listed[0].id, "c-new", "newest first");

        assert_eq!(store.prune_checkpoints(now, 30).unwrap(), 1);
        let listed = store.checkpoints_for_session(&sid).unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id, "c-new");
        // 0 keeps forever.
        assert_eq!(store.prune_checkpoints(now, 0).unwrap(), 0);
        assert_eq!(store.checkpoints_for_session(&sid).unwrap().len(), 1);

        let keys = store.checkpoint_dir_keys().unwrap();
        assert_eq!(keys.len(), 1);
        assert!(keys[0].ends_with(&format!("/{}", &sid.0[(sid.0.len() - 8)..])));
    }

    #[test]
    fn same_id_replay_is_ignored() {
        let store = Store::open_in_memory().unwrap();
        store.insert_session(&session()).unwrap();
        let sid = store
            .list_recent_sessions(1)
            .unwrap()
            .pop()
            .map(|s| s.id)
            .unwrap();
        let mut r = row(&sid, "x", 5);
        store.insert_checkpoint(&r).unwrap();
        r.sha = None;
        store.insert_checkpoint(&r).unwrap();
        assert_eq!(store.checkpoints_for_session(&sid).unwrap().len(), 1);
    }
}
