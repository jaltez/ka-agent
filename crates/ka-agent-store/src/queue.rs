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
