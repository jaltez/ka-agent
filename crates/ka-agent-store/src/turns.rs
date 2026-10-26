//! Turn persistence, usage accounting and the FTS5 episode index.
//! Split from lib.rs (moves only).

use super::*;

impl Store {
    /// Persist a brand-new turn (status `queued`, gap G8 mitigation).
    pub fn insert_turn(&self, t: &Turn) -> Result<()> {
        let conn = self.lock();
        conn.execute(
            "INSERT INTO turns(id, session, inbound_text, status, stop, error_class, error_message, cost_usd, tokens, created_ms, updated_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            params![
                t.id.as_str(),
                t.session.as_str(),
                t.inbound_text,
                status_str(&t.status),
                t.stop,
                t.error_class,
                t.error_message,
                t.cost_usd,
                t.tokens as i64,
                t.created_ms,
                t.updated_ms
            ],
        )?;
        Ok(())
    }

    /// Overwrite a turn after validating `in_db_status -> t.status` against
    /// the core turn state machine. Terminal fields (stop, errors, usage)
    /// travel with the struct; the caller keeps the authoritative copy.
    pub fn update_turn(&self, t: &Turn) -> Result<()> {
        let mut conn = self.lock();
        let tx = conn.transaction()?;
        let current: Option<String> = tx
            .query_row(
                "SELECT status FROM turns WHERE id = ?1",
                params![t.id.as_str()],
                |r| r.get(0),
            )
            .optional()?;
        let Some(current) = current else {
            return Err(StoreError::NotFound(format!("turn {}", t.id)));
        };
        let Some(current) = parse_turn_status(&current) else {
            return Err(StoreError::NotFound(format!("turn {}", t.id)));
        };
        // A same-status write is a payload refresh, not a transition: ka
        // cycles can emit several `turn_finished` events (auto-poke, verify)
        // and the later ones carry the cumulative usage.
        if current != t.status && !current.can_transition_to(t.status) {
            return Err(StoreError::IllegalTransition {
                what: "turn",
                from: status_str(&current),
                to: status_str(&t.status),
            });
        }
        tx.execute(
            "UPDATE turns SET status = ?2, stop = ?3, error_class = ?4, error_message = ?5,
                    cost_usd = ?6, tokens = ?7, updated_ms = ?8
             WHERE id = ?1",
            params![
                t.id.as_str(),
                status_str(&t.status),
                t.stop,
                t.error_class,
                t.error_message,
                t.cost_usd,
                t.tokens as i64,
                t.updated_ms
            ],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Append to a turn's inbound text (a message steered into its live
    /// turn). `inbound_text` is otherwise immutable.
    pub fn append_turn_inbound(&self, id: &TurnId, suffix: &str) -> Result<()> {
        let conn = self.lock();
        let changed = conn.execute(
            "UPDATE turns SET inbound_text = inbound_text || ?2 WHERE id = ?1",
            params![id.as_str(), suffix],
        )?;
        if changed == 0 {
            return Err(StoreError::NotFound(format!("turn {}", id)));
        }
        Ok(())
    }

    /// Restart reconciliation: any turn still `submitted` or `running` when
    /// the daemon comes up is marked `interrupted` (gap G9). `queued` turns
    /// survive — they have not been handed to the engine yet. Returns the
    /// interrupted ids.
    pub fn reconcile_running_turns(&self, now_ms: i64) -> Result<Vec<TurnId>> {
        let mut conn = self.lock();
        let tx = conn.transaction()?;
        let mut stmt = tx.prepare(
            "SELECT id FROM turns WHERE status IN ('submitted','running') ORDER BY created_ms",
        )?;
        let ids = stmt
            .query_map([], |r| r.get::<_, String>(0).map(TurnId))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        drop(stmt);
        tx.execute(
            "UPDATE turns SET status = 'interrupted', updated_ms = ?1
             WHERE status IN ('submitted','running')",
            params![now_ms],
        )?;
        tx.commit()?;
        Ok(ids)
    }

    /// Newest turns for one session (returned oldest-first within the
    /// window).
    pub fn latest_turns_for_session(&self, session: &SessionId, limit: usize) -> Result<Vec<Turn>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT id, session, inbound_text, status, stop, error_class, error_message,
                    cost_usd, tokens, created_ms, updated_ms
             FROM turns WHERE session = ?1 ORDER BY created_ms DESC LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![session.as_str(), limit as i64], row_to_turn)?;
        let mut out = rows.collect::<std::result::Result<Vec<_>, _>>()?;
        out.reverse();
        Ok(out)
    }

    /// One turn by id, if it exists.
    pub fn get_turn(&self, id: &TurnId) -> Result<Option<Turn>> {
        let conn = self.lock();
        let turn = conn
            .query_row(
                "SELECT id, session, inbound_text, status, stop, error_class, error_message,
                        cost_usd, tokens, created_ms, updated_ms
                 FROM turns WHERE id = ?1",
                params![id.as_str()],
                row_to_turn,
            )
            .optional()?;
        Ok(turn)
    }

    /// Most recent turn whose id starts with `prefix` — `@turn:<prefix>`
    /// references resolve through this. Global, not per-session: one
    /// instance is one operator (SEED.md §8.1).
    /// A turn by full id, prefix (with or without `turn_`) or the
    /// [`short_id`](ka_agent_core::ids::short_id) tail; newest wins.
    pub fn find_turn_by_prefix(&self, prefix: &str) -> Result<Option<Turn>> {
        let conn = self.lock();
        let turn = conn
            .query_row(
                "SELECT id, session, inbound_text, status, stop, error_class, error_message,
                        cost_usd, tokens, created_ms, updated_ms
                 FROM turns
                 WHERE substr(id, 1, length(?1)) = ?1
                    OR substr(id, 6, length(?1)) = ?1
                    OR (length(?1) >= 4 AND substr(id, -length(?1)) = ?1)
                 ORDER BY created_ms DESC LIMIT 1",
                params![prefix],
                row_to_turn,
            )
            .optional()?;
        Ok(turn)
    }

    /// Sum of `cost_usd` and `tokens` over turns created in
    /// `[day_start_ms, day_end_ms)` — the budget window query.
    pub fn sum_usage(&self, day_start_ms: i64, day_end_ms: i64) -> Result<(f64, u64)> {
        let conn = self.lock();
        let (cost, tokens): (Option<f64>, Option<i64>) = conn.query_row(
            "SELECT SUM(cost_usd), SUM(tokens) FROM turns
             WHERE created_ms >= ?1 AND created_ms < ?2",
            params![day_start_ms, day_end_ms],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        Ok((cost.unwrap_or(0.0), tokens.unwrap_or(0).max(0) as u64))
    }

    /// One row per finished turn since `since_ms`: `(created_ms, cost_usd,
    /// tokens)` — everything `ka-agent usage` needs; the CLI buckets days
    /// in the operator's timezone.
    pub fn usage_rows_since(&self, since_ms: i64) -> Result<Vec<(i64, f64, u64)>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT created_ms, COALESCE(cost_usd, 0), COALESCE(tokens, 0)
             FROM turns WHERE created_ms >= ?1 ORDER BY created_ms",
        )?;
        let rows = stmt
            .query_map(params![since_ms], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, f64>(1)?,
                    r.get::<_, i64>(2)?.max(0) as u64,
                ))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    // ------------------------------------------------------------------
    // episodes (FTS5)
    // ------------------------------------------------------------------

    /// Index one episode message for search.
    pub fn index_episode(
        &self,
        session: &SessionId,
        turn: &TurnId,
        role: &str,
        content: &str,
    ) -> Result<()> {
        let conn = self.lock();
        conn.execute(
            "INSERT INTO episodes_fts(session, turn, role, content) VALUES (?1, ?2, ?3, ?4)",
            params![session.as_str(), turn.as_str(), role, content],
        )?;
        Ok(())
    }

    /// Phrase-quoted FTS search (like ka-index: the raw query is wrapped in
    /// double quotes with inner quotes doubled, so it is never interpreted
    /// as FTS syntax). Returns hits with a snippet around the match.
    pub fn search_episodes(&self, query: &str, limit: usize) -> Result<Vec<EpisodeHit>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT f.session, f.turn, f.role,
                    snippet(episodes_fts, 3, '[', ']', '…', 12), t.created_ms
             FROM episodes_fts f
             LEFT JOIN turns t ON t.id = f.turn
             WHERE episodes_fts MATCH ?1
             ORDER BY rank LIMIT ?2",
        )?;
        let quoted = format!("\"{}\"", query.replace('"', "\"\""));
        let rows = stmt.query_map(params![quoted, limit as i64], |r| {
            Ok(EpisodeHit {
                session: r.get(0)?,
                turn: r.get(1)?,
                role: r.get(2)?,
                snippet: r.get(3)?,
                at_ms: r.get(4).unwrap_or(0),
            })
        })?;
        collect_rows(rows)
    }

    /// Remove a session's episodes (strand reset / purge).
    pub fn delete_session_episodes(&self, session: &SessionId) -> Result<()> {
        let conn = self.lock();
        conn.execute(
            "DELETE FROM episodes_fts WHERE session = ?1",
            params![session.as_str()],
        )?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[test]
    fn turn_reconcile_marks_only_in_flight() {
        let store = Store::open_in_memory().unwrap();
        let s = session();
        store.insert_session(&s).unwrap();
        let mut queued = Turn::new(s.id.clone(), "q".into());
        let mut submitted = Turn::new(s.id.clone(), "s".into());
        let mut running = Turn::new(s.id.clone(), "r".into());
        submitted.status = TurnStatus::Submitted;
        running.status = TurnStatus::Running;
        store.insert_turn(&queued).unwrap();
        store.insert_turn(&submitted).unwrap();
        store.insert_turn(&running).unwrap();

        let ids = store.reconcile_running_turns(1234).unwrap();
        assert_eq!(ids.len(), 2);
        let after = store.latest_turns_for_session(&s.id, 10).unwrap();
        let q = after.iter().find(|t| t.inbound_text == "q").unwrap();
        let sub = after.iter().find(|t| t.inbound_text == "s").unwrap();
        let run = after.iter().find(|t| t.inbound_text == "r").unwrap();
        assert_eq!(q.status, TurnStatus::Queued);
        assert_eq!(sub.status, TurnStatus::Interrupted);
        assert_eq!(run.status, TurnStatus::Interrupted);

        // update_turn legality against the in-DB status
        let mut done = run.clone();
        done.status = TurnStatus::Done; // interrupted -> done is illegal
        let err = store.update_turn(&done).unwrap_err();
        assert!(matches!(err, StoreError::IllegalTransition { .. }), "{err}");
        // queued -> submitted via update_turn is legal
        queued.status = TurnStatus::Submitted;
        store.update_turn(&queued).unwrap();
    }

    #[test]
    fn sum_usage_window_math() {
        let store = Store::open_in_memory().unwrap();
        let s = session();
        store.insert_session(&s).unwrap();
        let day0 = 86_400_000i64 * 50; // arbitrary day start
        for (i, cost) in [(0i64, 1.5f64), (1, 2.5), (2, 10.0)] {
            let mut t = Turn::new(s.id.clone(), format!("t{i}"));
            t.created_ms = day0 + i * 1000;
            t.cost_usd = cost;
            t.tokens = (i as u64 + 1) * 100;
            store.insert_turn(&t).unwrap();
        }
        // window covers the first two turns only
        let (cost, tokens) = store.sum_usage(day0, day0 + 1500).unwrap();
        assert!((cost - 4.0).abs() < 1e-9, "{cost}");
        assert_eq!(tokens, 300);
        // full window
        let (cost, tokens) = store.sum_usage(day0, day0 + 86_400_000).unwrap();
        assert!((cost - 14.0).abs() < 1e-9, "{cost}");
        assert_eq!(tokens, 600);
        // empty window
        assert_eq!(store.sum_usage(0, 1).unwrap(), (0.0, 0));
    }

    #[test]
    fn episodes_index_search_delete() {
        let store = Store::open_in_memory().unwrap();
        let s1 = session();
        let s2 = Session::new(std::path::PathBuf::from("/tmp/ws2"));
        store.insert_session(&s1).unwrap();
        store.insert_session(&s2).unwrap();
        let t = TurnId::generate();
        store
            .index_episode(&s1.id, &t, "user", "fix the flaky mutex test")
            .unwrap();
        store
            .index_episode(&s1.id, &t, "assistant", "the mutex was double-locked")
            .unwrap();
        store
            .index_episode(&s2.id, &t, "user", "unrelated grocery list")
            .unwrap();

        let hits = store.search_episodes("mutex", 10).unwrap();
        assert_eq!(hits.len(), 2);
        assert!(hits.iter().all(|h| h.session == *s1.id.as_str()));

        store.delete_session_episodes(&s1.id).unwrap();
        assert_eq!(store.search_episodes("mutex", 10).unwrap().len(), 0);
        assert_eq!(store.search_episodes("grocery", 10).unwrap().len(), 1);
    }
}
