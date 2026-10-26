//! The outbound chunk outbox: crash-safe delivery bookkeeping.
//! Split from lib.rs (moves only).

use super::*;

impl Store {
    /// Insert (or replace, on retry) an outbound chunk keyed by
    /// `(turn, chunk_index)` — a crash mid-send can neither reorder nor
    /// duplicate beyond the chunk in flight.
    pub fn push_chunk(&self, chunk: &OutboxChunk) -> Result<()> {
        let conn = self.lock();
        conn.execute(
            "INSERT OR REPLACE INTO outbox(turn, chunk_index, text, status, channel_message_id, created_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                chunk.turn.as_str(),
                chunk.chunk_index as i64,
                chunk.text,
                status_str(&chunk.status),
                chunk.channel_message_id,
                chunk.created_ms
            ],
        )?;
        Ok(())
    }

    /// Unsent chunks for a turn, in order.
    /// Every undelivered chunk for a session, oldest turn first — the
    /// crash-recovery read a channel drains at startup (SEED §3.7).
    pub fn pending_chunks_for_session(
        &self,
        session: &SessionId,
    ) -> Result<Vec<OutboxChunk>, StoreError> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT o.turn, o.chunk_index, o.text, o.status, o.channel_message_id, o.created_ms
             FROM outbox o JOIN turns t ON t.id = o.turn
             WHERE t.session = ?1 AND o.status = 'queued'
             ORDER BY o.created_ms, o.turn, o.chunk_index",
        )?;
        let rows = stmt.query_map(params![session.as_str()], |r| {
            Ok(OutboxChunk {
                turn: TurnId(r.get::<_, String>(0)?),
                chunk_index: r.get::<_, i64>(1)? as u32,
                text: r.get(2)?,
                status: parse_outbox_status(&r.get::<_, String>(3)?)
                    .unwrap_or(OutboxStatus::Failed),
                channel_message_id: r.get(4)?,
                created_ms: r.get(5)?,
            })
        })?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    /// Undelivered chunks for one turn (store tests).
    pub fn pending_chunks(&self, turn: &TurnId) -> Result<Vec<OutboxChunk>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT turn, chunk_index, text, status, channel_message_id, created_ms
             FROM outbox WHERE turn = ?1 AND status = 'queued'
             ORDER BY chunk_index",
        )?;
        let rows = stmt.query_map(params![turn.as_str()], |r| {
            Ok(OutboxChunk {
                turn: TurnId(r.get::<_, String>(0)?),
                chunk_index: r.get::<_, i64>(1)? as u32,
                text: r.get(2)?,
                status: parse_outbox_status(&r.get::<_, String>(3)?)
                    .unwrap_or(OutboxStatus::Failed),
                channel_message_id: r.get(4)?,
                created_ms: r.get(5)?,
            })
        })?;
        collect_rows(rows)
    }

    /// Every chunk for a turn (any status), in order — history rendering.
    pub fn chunks_for_turn(&self, turn: &TurnId) -> Result<Vec<OutboxChunk>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT turn, chunk_index, text, status, channel_message_id, created_ms
             FROM outbox WHERE turn = ?1
             ORDER BY chunk_index",
        )?;
        let rows = stmt.query_map(params![turn.as_str()], |r| {
            Ok(OutboxChunk {
                turn: TurnId(r.get::<_, String>(0)?),
                chunk_index: r.get::<_, i64>(1)? as u32,
                text: r.get(2)?,
                status: parse_outbox_status(&r.get::<_, String>(3)?)
                    .unwrap_or(OutboxStatus::Failed),
                channel_message_id: r.get(4)?,
                created_ms: r.get(5)?,
            })
        })?;
        collect_rows(rows)
    }

    /// Mark a chunk delivered with its channel message id.
    pub fn mark_sent(
        &self,
        turn: &TurnId,
        chunk_index: u32,
        channel_message_id: i64,
    ) -> Result<()> {
        let conn = self.lock();
        let changed = conn.execute(
            "UPDATE outbox SET status = 'sent', channel_message_id = ?3
             WHERE turn = ?1 AND chunk_index = ?2",
            params![turn.as_str(), chunk_index as i64, channel_message_id],
        )?;
        if changed == 0 {
            return Err(StoreError::NotFound(format!(
                "outbox chunk {}/{}",
                turn, chunk_index
            )));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[test]
    fn outbox_ordering_and_mark_sent() {
        let store = Store::open_in_memory().unwrap();
        let turn = TurnId::generate();
        store
            .push_chunk(&OutboxChunk::new(turn.clone(), 1, "b".into()))
            .unwrap();
        store
            .push_chunk(&OutboxChunk::new(turn.clone(), 0, "a".into()))
            .unwrap();
        let pending = store.pending_chunks(&turn).unwrap();
        assert_eq!(pending.len(), 2);
        assert_eq!(pending[0].text, "a");
        assert_eq!(pending[1].text, "b");
        store.mark_sent(&turn, 0, 4242).unwrap();
        let pending = store.pending_chunks(&turn).unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].text, "b");
        let err = store.mark_sent(&turn, 9, 1).unwrap_err();
        assert!(matches!(err, StoreError::NotFound { .. }), "{err}");
    }

    #[test]
    fn pending_chunks_for_session_orders_and_scopes() {
        let store = Store::open_in_memory().unwrap();
        let s1 = session();
        let s2 = session();
        store.insert_session(&s1).unwrap();
        store.insert_session(&s2).unwrap();
        let mut t1 = Turn::new(s1.id.clone(), "one".into());
        store.insert_turn(&t1).unwrap();
        t1.transition(TurnStatus::Submitted).unwrap();
        store.update_turn(&t1).unwrap();
        t1.transition(TurnStatus::Running).unwrap();
        store.update_turn(&t1).unwrap();
        let mut t2 = Turn::new(s2.id.clone(), "two".into());
        store.insert_turn(&t2).unwrap();
        t2.transition(TurnStatus::Submitted).unwrap();
        store.update_turn(&t2).unwrap();
        // Two queued chunks in s1, one sent; one in s2.
        store
            .push_chunk(&OutboxChunk::new(t1.id.clone(), 0, "a".into()))
            .unwrap();
        store
            .push_chunk(&OutboxChunk::new(t1.id.clone(), 1, "b".into()))
            .unwrap();
        store.mark_sent(&t1.id, 0, 1).unwrap();
        store
            .push_chunk(&OutboxChunk::new(t2.id.clone(), 0, "other".into()))
            .unwrap();
        let mine = store.pending_chunks_for_session(&s1.id).unwrap();
        assert_eq!(mine.len(), 1, "sent chunks are not pending");
        assert_eq!(mine[0].text, "b");
        assert_eq!(mine[0].turn, t1.id);
        let theirs = store.pending_chunks_for_session(&s2.id).unwrap();
        assert_eq!(theirs.len(), 1, "another session's chunks never leak");
        assert_eq!(theirs[0].text, "other");
    }
}
