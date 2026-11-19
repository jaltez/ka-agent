//! The durable inbound message queue (gap G4).
//! Split from lib.rs (moves only).

use super::*;

impl Store {
    /// Enqueue a durable inbound message (gap G4).
    pub fn enqueue(&self, item: &QueueItem) -> Result<()> {
        let conn = self.lock();
        conn.execute(
            "INSERT INTO queue(id, session, text, source, enqueued_ms, turned)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                item.id.as_str(),
                item.session.as_str(),
                item.text,
                item.source,
                item.enqueued_ms,
                item.turned.as_ref().map(|t| t.as_str())
            ],
        )?;
        Ok(())
    }

    /// Oldest not-yet-turned queue item for the session (FIFO).
    pub fn next_queued(&self, session: &SessionId) -> Result<Option<QueueItem>> {
        let conn = self.lock();
        conn.query_row(
            "SELECT id, session, text, source, enqueued_ms, turned
             FROM queue WHERE session = ?1 AND turned IS NULL
             ORDER BY enqueued_ms, id LIMIT 1",
            params![session.as_str()],
            |r| {
                Ok(QueueItem {
                    id: QueueId(r.get::<_, String>(0)?),
                    session: SessionId(r.get::<_, String>(1)?),
                    text: r.get(2)?,
                    source: r.get(3)?,
                    enqueued_ms: r.get(4)?,
                    turned: r.get::<_, Option<String>>(5)?.map(TurnId),
                })
            },
        )
        .optional()
        .map_err(StoreError::from)
    }

    /// Every not-yet-turned queue item for the session, oldest first
    /// (what `handle_message` must not overtake, and `/status` shows).
    pub fn queued_items(&self, session: &SessionId) -> Result<Vec<QueueItem>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT id, session, text, source, enqueued_ms, turned
             FROM queue WHERE session = ?1 AND turned IS NULL
             ORDER BY enqueued_ms, id",
        )?;
        let rows = stmt.query_map(params![session.as_str()], |r| {
            Ok(QueueItem {
                id: QueueId(r.get::<_, String>(0)?),
                session: SessionId(r.get::<_, String>(1)?),
                text: r.get(2)?,
                source: r.get(3)?,
                enqueued_ms: r.get(4)?,
                turned: r.get::<_, Option<String>>(5)?.map(TurnId),
            })
        })?;
        collect_rows(rows)
    }

    /// Remove one queued item by exact id (`/queue` per-item removal):
    /// `true` when a row went away, `false` when nothing matched.
    pub fn queue_remove(&self, id: &QueueId) -> Result<bool> {
        let conn = self.lock();
        Ok(conn.execute("DELETE FROM queue WHERE id = ?1", params![id.as_str()])? > 0)
    }

    /// Enqueue at the FRONT of the session's pending queue (A2 `/queue
    /// top`/`now`): the new row stamps `head_ms - 1` so it sorts before
    /// the current head (`enqueued_ms, id` order). No schema change.
    pub fn enqueue_front(&self, item: &QueueItem) -> Result<()> {
        let conn = self.lock();
        let head: Option<i64> = conn
            .query_row(
                "SELECT MIN(enqueued_ms) FROM queue WHERE session = ?1 AND turned IS NULL",
                params![item.session.as_str()],
                |r| r.get(0),
            )
            .optional()
            .map_err(StoreError::from)?
            .flatten();
        let mut stamped = item.clone();
        stamped.enqueued_ms = head.unwrap_or_else(ka_agent_core::now_ms) - 1;
        conn.execute(
            "INSERT INTO queue(id, session, text, source, enqueued_ms, turned)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                stamped.id.as_str(),
                stamped.session.as_str(),
                stamped.text,
                stamped.source,
                stamped.enqueued_ms,
                stamped.turned.as_ref().map(|t| t.as_str())
            ],
        )?;
        Ok(())
    }

    /// Rewrite one pending item's text (A2 `/queue edit`). Pending rows
    /// only — a turned item's text is history and must not change.
    /// `Ok(false)` when the id is unknown or already turned.
    pub fn queue_set_text(&self, id: &QueueId, text: &str) -> Result<bool> {
        let conn = self.lock();
        let n = conn.execute(
            "UPDATE queue SET text = ?2 WHERE id = ?1 AND turned IS NULL",
            params![id.as_str(), text],
        )?;
        Ok(n > 0)
    }

    /// Reorder the session's pending block so the named item lands at
    /// 1-based position `to_pos` (A2 `/queue top|up|down`): the pending
    /// rows' `enqueued_ms` are rewritten oldest-first in the new order
    /// inside one transaction. `Ok(false)` when `id` is not a pending row
    /// of this session or `to_pos` is out of range.
    pub fn queue_reorder(&self, session: &SessionId, id: &QueueId, to_pos: usize) -> Result<bool> {
        let conn = self.lock();
        let mut pending: Vec<(QueueId, i64)> = {
            let mut stmt = conn.prepare(
                "SELECT id, enqueued_ms FROM queue
                 WHERE session = ?1 AND turned IS NULL
                 ORDER BY enqueued_ms, id",
            )?;
            let rows = stmt.query_map(params![session.as_str()], |r| {
                Ok((QueueId(r.get::<_, String>(0)?), r.get(1)?))
            })?;
            rows.collect::<std::result::Result<Vec<_>, _>>()?
        };
        let Some(from) = pending.iter().position(|(qid, _)| qid == id) else {
            return Ok(false);
        };
        let to = to_pos.saturating_sub(1).min(pending.len() - 1);
        let moved = pending.remove(from);
        let to = to.min(pending.len());
        pending.insert(to, moved);
        let tx = conn.unchecked_transaction()?;
        for (pos, (qid, _)) in pending.iter().enumerate() {
            tx.execute(
                "UPDATE queue SET enqueued_ms = ?2 WHERE id = ?1",
                params![qid.as_str(), pos as i64],
            )?;
        }
        tx.commit()?;
        Ok(true)
    }

    /// Record that a queue item became a turn.
    pub fn mark_turned(&self, id: &QueueId, turn: &TurnId) -> Result<()> {
        let conn = self.lock();
        let changed = conn.execute(
            "UPDATE queue SET turned = ?2 WHERE id = ?1",
            params![id.as_str(), turn.as_str()],
        )?;
        if changed == 0 {
            return Err(StoreError::NotFound(format!("queue item {}", id)));
        }
        Ok(())
    }

    /// Number of not-yet-turned queue items for the session.
    pub fn queue_len(&self, session: &SessionId) -> Result<usize> {
        let conn = self.lock();
        let n: i64 = conn.query_row(
            "SELECT COUNT(*) FROM queue WHERE session = ?1 AND turned IS NULL",
            params![session.as_str()],
            |r| r.get(0),
        )?;
        Ok(n as usize)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[test]
    fn queue_fifo_and_mark_turned() {
        let store = Store::open_in_memory().unwrap();
        let s = session();
        store.insert_session(&s).unwrap();
        let a = QueueItem::new(s.id.clone(), "first".into(), "telegram:1");
        let mut b = QueueItem::new(s.id.clone(), "second".into(), "telegram:1");
        b.enqueued_ms += 10; // deterministic order regardless of clock granularity
        store.enqueue(&a).unwrap();
        store.enqueue(&b).unwrap();
        assert_eq!(store.queue_len(&s.id).unwrap(), 2);
        let next = store.next_queued(&s.id).unwrap().unwrap();
        assert_eq!(next.text, "first");
        let t = TurnId::generate();
        store.mark_turned(&next.id, &t).unwrap();
        assert_eq!(store.queue_len(&s.id).unwrap(), 1);
        let next = store.next_queued(&s.id).unwrap().unwrap();
        assert_eq!(next.text, "second");
        assert_eq!(next.turned, None);
    }
}
