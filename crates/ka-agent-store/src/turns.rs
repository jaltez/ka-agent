//! Turn persistence, usage accounting and the FTS5 episode index.
//! Split from lib.rs (moves only).

use super::*;

/// One admin usage row: `(created_ms, cost, tokens, model, username)`.
#[allow(dead_code)]
pub type UserUsageRow = (i64, f64, u64, Option<String>, String);

use rusqlite::params_from_iter;

/// Leading-position `/history` filters (B17 power-user half): bound SQL
/// predicates on the `turns` join — filter values never enter the FTS
/// MATCH expression.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EpisodeFilter {
    pub model: Option<String>,
    /// Inclusive lower bound on `turns.created_ms`.
    pub after_ms: Option<i64>,
    /// One of the turn statuses (`queued`, `done`, `error`, …).
    pub status: Option<String>,
}

/// The `AND t.… = ?N` predicates for an [`EpisodeFilter`], binding into
/// `binds` (all TEXT; the integer comparison casts at the column).
fn filter_preds(filter: &EpisodeFilter, binds: &mut Vec<String>) -> String {
    let mut sql = String::new();
    if let Some(model) = &filter.model {
        binds.push(model.clone());
        sql.push_str(&format!(" AND t.model = ?{}", binds.len()));
    }
    if let Some(after) = filter.after_ms {
        binds.push(after.to_string());
        sql.push_str(&format!(
            " AND t.created_ms >= CAST(?{} AS INTEGER)",
            binds.len()
        ));
    }
    if let Some(status) = &filter.status {
        binds.push(status.clone());
        sql.push_str(&format!(" AND t.status = ?{}", binds.len()));
    }
    sql
}

impl Store {
    /// Persist a brand-new turn (status `queued`, gap G8 mitigation).
    pub fn insert_turn(&self, t: &Turn) -> Result<()> {
        let conn = self.lock();
        conn.execute(
            "INSERT INTO turns(id, session, inbound_text, status, stop, error_class, error_message, cost_usd, tokens, model, created_ms, updated_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
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
                t.model,
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
    /// window), skipping the `offset` newest rows first — paging for the
    /// history picker.
    pub fn latest_turns_for_session(
        &self,
        session: &SessionId,
        limit: usize,
        offset: usize,
    ) -> Result<Vec<Turn>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT id, session, inbound_text, status, stop, error_class, error_message,
                    cost_usd, tokens, model, created_ms, updated_ms
             FROM turns WHERE session = ?1 ORDER BY created_ms DESC LIMIT ?2 OFFSET ?3",
        )?;
        let rows = stmt.query_map(
            params![session.as_str(), limit as i64, offset as i64],
            row_to_turn,
        )?;
        let mut out = rows.collect::<std::result::Result<Vec<_>, _>>()?;
        out.reverse();
        Ok(out)
    }

    /// Number of turns recorded for one session — the `@session:` header.
    pub fn count_turns_for_session(&self, session: &SessionId) -> Result<i64> {
        let conn = self.lock();
        let count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM turns WHERE session = ?1",
            params![session.as_str()],
            |r| r.get(0),
        )?;
        Ok(count)
    }

    /// One turn by id, if it exists.
    pub fn get_turn(&self, id: &TurnId) -> Result<Option<Turn>> {
        let conn = self.lock();
        let turn = conn
            .query_row(
                "SELECT id, session, inbound_text, status, stop, error_class, error_message,
                        cost_usd, tokens, model, created_ms, updated_ms
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
                        cost_usd, tokens, model, created_ms, updated_ms
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
    /// tokens, model)` — everything `ka-agent usage` needs; the CLI buckets
    /// days in the operator's timezone and splits spend per model.
    pub fn usage_rows_since(&self, since_ms: i64) -> Result<Vec<UsageRow>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT created_ms, COALESCE(cost_usd, 0), COALESCE(tokens, 0), model
             FROM turns WHERE created_ms >= ?1 ORDER BY created_ms",
        )?;
        let rows = stmt
            .query_map(params![since_ms], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, f64>(1)?,
                    r.get::<_, i64>(2)?.max(0) as u64,
                    r.get::<_, Option<String>>(3)?,
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

    /// FTS search over episodes. The raw query is tokenized on whitespace
    /// and each token is double-quoted (inner quotes doubled) and joined
    /// with `" AND "` — every word must appear, in any order, and nothing
    /// is ever interpreted as FTS syntax. A double-quoted span in the
    /// input stays one token, so an exact phrase is still searchable.
    /// Returns hits with a snippet around the match.
    pub fn search_episodes(&self, query: &str, limit: usize) -> Result<Vec<EpisodeHit>> {
        self.search_episodes_filtered(query, limit, &EpisodeFilter::default())
    }

    /// [`search_episodes`](Self::search_episodes) with an
    /// [`EpisodeFilter`] applied to the `turns` join. Under a filter the
    /// LEFT JOIN is effectively inner: episodes whose turn row was pruned
    /// drop out silently (accepted).
    pub fn search_episodes_filtered(
        &self,
        query: &str,
        limit: usize,
        filter: &EpisodeFilter,
    ) -> Result<Vec<EpisodeHit>> {
        let match_expr = fts_and_query(query);
        if match_expr.is_empty() {
            return Ok(Vec::new());
        }
        let mut binds = vec![match_expr];
        let preds = filter_preds(filter, &mut binds);
        let sql = format!(
            "SELECT f.session, f.turn, f.role,
                    snippet(episodes_fts, 3, '[', ']', '…', 12), t.created_ms
             FROM episodes_fts f LEFT JOIN turns t ON t.id = f.turn
             WHERE episodes_fts MATCH ?1 {preds}
             ORDER BY rank LIMIT ?{}",
            binds.len() + 1,
        );
        binds.push(limit.to_string());
        let conn = self.lock();
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(params_from_iter(binds.iter()), |r| {
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

    /// Episodes of one role's sessions only (`admin` sees everything): a
    /// subject never reads another role's conversation history.
    pub fn search_episodes_for_role(
        &self,
        query: &str,
        limit: usize,
        role: &str,
    ) -> Result<Vec<EpisodeHit>> {
        let match_expr = fts_and_query(query);
        if match_expr.is_empty() {
            return Ok(Vec::new());
        }
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT f.session, f.turn, f.role,
                    snippet(episodes_fts, 3, '[', ']', '…', 12), t.created_ms
             FROM episodes_fts f
             LEFT JOIN turns t ON t.id = f.turn
             WHERE episodes_fts MATCH ?1
               AND f.session IN (SELECT id FROM sessions WHERE role = ?3)
             ORDER BY rank LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![match_expr, limit as i64, role], |r| {
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

    /// Usage rows with their user (the admin's per-user dashboard):
    /// `(created_ms, cost_usd, tokens, model, username)`.
    pub fn usage_rows_with_user(&self, since_ms: i64) -> Result<Vec<UserUsageRow>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT t.created_ms, t.cost_usd, t.tokens, t.model, COALESCE(s.username, 'operator')
             FROM turns t LEFT JOIN sessions s ON s.id = t.session
             WHERE t.created_ms >= ?1 ORDER BY t.created_ms",
        )?;
        let rows = stmt
            .query_map(params![since_ms], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, f64>(1)?,
                    r.get::<_, i64>(2)?.max(0) as u64,
                    r.get::<_, Option<String>>(3)?,
                    r.get::<_, String>(4)?,
                ))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Episodes of one user's sessions only (a user never reads another's
    /// conversation history; admin callers use [`Self::search_episodes`]).
    pub fn search_episodes_for_user(
        &self,
        query: &str,
        limit: usize,
        username: &str,
    ) -> Result<Vec<EpisodeHit>> {
        self.search_episodes_for_user_filtered(query, limit, username, &EpisodeFilter::default())
    }

    /// [`search_episodes_for_user`](Self::search_episodes_for_user) with
    /// an [`EpisodeFilter`] on the `turns` join.
    pub fn search_episodes_for_user_filtered(
        &self,
        query: &str,
        limit: usize,
        username: &str,
        filter: &EpisodeFilter,
    ) -> Result<Vec<EpisodeHit>> {
        let match_expr = fts_and_query(query);
        if match_expr.is_empty() {
            return Ok(Vec::new());
        }
        let mut binds = vec![match_expr, username.to_string()];
        let preds = filter_preds(filter, &mut binds);
        let sql = format!(
            "SELECT f.session, f.turn, f.role,
                    snippet(episodes_fts, 3, '[', ']', '…', 12), t.created_ms
             FROM episodes_fts f LEFT JOIN turns t ON t.id = f.turn
             WHERE episodes_fts MATCH ?1
               AND f.session IN (SELECT id FROM sessions WHERE username = ?2)
               {preds}
             ORDER BY rank LIMIT ?{}",
            binds.len() + 1,
        );
        binds.push(limit.to_string());
        let conn = self.lock();
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(params_from_iter(binds.iter()), |r| {
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

    /// The user the session of a turn belongs to (`None` for an unknown
    /// turn).
    pub fn turn_username(&self, id: &TurnId) -> Result<Option<String>> {
        let conn = self.lock();
        conn.query_row(
            "SELECT s.username FROM turns t JOIN sessions s ON s.id = t.session WHERE t.id = ?1",
            params![id.as_str()],
            |r| r.get(0),
        )
        .optional()
        .map_err(StoreError::from)
    }

    /// The role of the session a turn belongs to (`None` for an unknown
    /// turn).
    pub fn turn_role(&self, id: &TurnId) -> Result<Option<String>> {
        let conn = self.lock();
        conn.query_row(
            "SELECT s.role FROM turns t JOIN sessions s ON s.id = t.session WHERE t.id = ?1",
            params![id.as_str()],
            |r| r.get(0),
        )
        .optional()
        .map_err(StoreError::from)
    }

    /// `updated_ms` of the user's newest turn that ended in a
    /// rate-limit error — the `memory_propose` cooldown clock (B15c);
    /// `None` when there is none.
    pub fn last_rate_limit_at(&self, username: &str) -> Result<Option<i64>> {
        let conn = self.lock();
        conn.query_row(
            "SELECT t.updated_ms FROM turns t JOIN sessions s ON s.id = t.session
             WHERE s.username = ?1 AND t.error_class = 'rate_limit'
             ORDER BY t.updated_ms DESC LIMIT 1",
            params![username],
            |r| r.get(0),
        )
        .optional()
        .map_err(StoreError::from)
    }

    /// Newest-first turn listing under an [`EpisodeFilter`] with no FTS
    /// text (`/history status:error`): the same bound predicates as the
    /// filtered search, `None` username = every user (the admin split).
    pub fn list_turns_filtered(
        &self,
        filter: &EpisodeFilter,
        username: Option<&str>,
        limit: usize,
    ) -> Result<Vec<EpisodeHit>> {
        let mut binds = Vec::new();
        let mut sql = String::from(
            "SELECT t.id, substr(COALESCE(t.inbound_text, ''), 1, 100),
                    t.created_ms, s.id, s.role
             FROM turns t JOIN sessions s ON s.id = t.session WHERE 1=1",
        );
        if let Some(username) = username {
            binds.push(username.to_string());
            sql.push_str(&format!(" AND s.username = ?{}", binds.len()));
        }
        sql.push_str(&filter_preds(filter, &mut binds));
        sql.push_str(&format!(
            " ORDER BY t.created_ms DESC LIMIT ?{}",
            binds.len() + 1
        ));
        binds.push(limit.to_string());
        let conn = self.lock();
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(params_from_iter(binds.iter()), |r| {
            Ok(EpisodeHit {
                session: r.get(3)?,
                turn: r.get(0)?,
                role: r.get(4)?,
                snippet: r.get(1)?,
                at_ms: r.get(2)?,
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

/// One FTS5 phrase per whitespace-separated token, ANDed together; a
/// double-quoted span of the input is kept as one token (its quotes become
/// literal characters, which the tokenizer treats as separators). Tokens
/// that are nothing but quotes are dropped, so an empty or quote-only
/// query yields no phrases and matches nothing.
pub(crate) fn fts_and_query(query: &str) -> String {
    let mut terms: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut in_quotes = false;
    let mut has_token = false;
    for c in query.chars() {
        match c {
            '"' => {
                in_quotes = !in_quotes;
                cur.push('"');
                has_token = true;
            }
            c if c.is_whitespace() && !in_quotes => {
                if has_token {
                    terms.push(std::mem::take(&mut cur));
                    has_token = false;
                }
            }
            c => {
                cur.push(c);
                has_token = true;
            }
        }
    }
    if has_token {
        terms.push(cur);
    }
    terms
        .iter()
        .filter(|t| !t.trim_matches('"').is_empty())
        .map(|t| format!("\"{}\"", t.replace('"', "\"\"")))
        .collect::<Vec<_>>()
        .join(" AND ")
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
        let after = store.latest_turns_for_session(&s.id, 10, 0).unwrap();
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

    /// E4: keywords AND together — two words that appear in different
    /// episodes match, a quoted span stays one phrase, and FTS operators
    /// in the input never execute.
    #[test]
    fn episodes_search_ands_terms_and_keeps_phrases() {
        let store = Store::open_in_memory().unwrap();
        let s = session();
        store.insert_session(&s).unwrap();
        let t = TurnId::generate();
        store
            .index_episode(&s.id, &t, "user", "please deploy nginx to staging")
            .unwrap();
        store
            .index_episode(&s.id, &t, "assistant", "check the cache hit rate first")
            .unwrap();
        store
            .index_episode(&s.id, &t, "assistant", "cache cleared — safe to deploy now")
            .unwrap();

        // Two keywords, not contiguous: an implicit AND inside one message.
        assert_eq!(store.search_episodes("deploy cache", 10).unwrap().len(), 1);
        // A missing keyword narrows to silence.
        assert_eq!(
            store.search_episodes("deploy postgres", 10).unwrap().len(),
            0
        );
        // A quoted phrase still matches contiguous words only.
        assert_eq!(
            store.search_episodes("\"deploy nginx\"", 10).unwrap().len(),
            1
        );
        assert_eq!(
            store.search_episodes("\"nginx deploy\"", 10).unwrap().len(),
            0
        );
        // FTS syntax in the input is data, never a query operator.
        assert_eq!(
            store.search_episodes("nginx OR cache", 10).unwrap().len(),
            0
        );
        assert_eq!(store.search_episodes("ngin*", 10).unwrap().len(), 0);
        // Nothing searchable: empty, not an error.
        assert!(store.search_episodes("", 10).unwrap().is_empty());
        assert!(store.search_episodes("\"\"", 10).unwrap().is_empty());
    }

    /// E5: the model a turn ran on round-trips through the store, and the
    /// usage rows carry it for the per-model split.
    #[test]
    fn usage_rows_carry_the_turn_model() {
        let store = Store::open_in_memory().unwrap();
        assert_eq!(store.user_version().unwrap(), MIGRATIONS.len());
        let s = session();
        store.insert_session(&s).unwrap();
        let mut a = Turn::new(s.id.clone(), "on the main model".into());
        a.model = Some("anthropic/claude-sonnet-4-5".into());
        a.cost_usd = 0.01;
        let mut b = Turn::new(s.id.clone(), "on the cheap one".into());
        b.model = Some("google/gemini-2.5-flash".into());
        b.cost_usd = 0.001;
        store.insert_turn(&a).unwrap();
        store.insert_turn(&b).unwrap();

        // Round-trip through every read path.
        assert_eq!(
            store.get_turn(&a.id).unwrap().unwrap().model.as_deref(),
            Some("anthropic/claude-sonnet-4-5")
        );
        // Usage rows carry the model per turn.
        let rows = store.usage_rows_since(0).unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].3.as_deref(), Some("anthropic/claude-sonnet-4-5"));
        assert_eq!(rows[1].3.as_deref(), Some("google/gemini-2.5-flash"));
    }

    #[test]
    fn last_rate_limit_at_is_per_user() {
        let store = Store::open_in_memory().unwrap();
        let mut s1 = session();
        s1.username = "alice".into();
        let mut s2 = session();
        s2.username = "bob".into();
        store.insert_session(&s1).unwrap();
        store.insert_session(&s2).unwrap();
        let mut err = Turn::new(s1.id.clone(), "hit the cap".into());
        err.status = TurnStatus::Error;
        err.error_class = Some("rate_limit".into());
        err.updated_ms = 5_000;
        let mut other = Turn::new(s2.id.clone(), "different user".into());
        other.status = TurnStatus::Error;
        other.error_class = Some("rate_limit".into());
        other.updated_ms = 9_000;
        let mut plain = Turn::new(s1.id.clone(), "ordinary".into());
        plain.status = TurnStatus::Done;
        plain.updated_ms = 8_000;
        store.insert_turn(&err).unwrap();
        store.insert_turn(&other).unwrap();
        store.insert_turn(&plain).unwrap();

        assert_eq!(store.last_rate_limit_at("alice").unwrap(), Some(5_000));
        assert_eq!(store.last_rate_limit_at("bob").unwrap(), Some(9_000));
        assert_eq!(store.last_rate_limit_at("carol").unwrap(), None);
    }

    #[test]
    fn episode_filters_bind_model_after_status() {
        let store = Store::open_in_memory().unwrap();
        let mut s_alice = session();
        s_alice.username = "alice".into();
        let mut s_bob = session();
        s_bob.username = "bob".into();
        store.insert_session(&s_alice).unwrap();
        store.insert_session(&s_bob).unwrap();

        let mut old = Turn::new(s_alice.id.clone(), "deploy the flaky mutex".into());
        old.model = Some("zai/glm-5.3".into());
        old.created_ms = 1_000;
        old.status = TurnStatus::Done;
        let mut err = Turn::new(s_alice.id.clone(), "flaky mutex again".into());
        err.model = Some("zai/glm-5.3".into());
        err.created_ms = 9_000;
        err.status = TurnStatus::Error;
        let mut bob = Turn::new(s_bob.id.clone(), "flaky mutex elsewhere".into());
        bob.model = Some("anthropic/claude-sonnet-4-5".into());
        bob.created_ms = 9_000;
        bob.status = TurnStatus::Done;
        for t in [&old, &err, &bob] {
            store.insert_turn(t).unwrap();
            store
                .index_episode(&s_of(t), &t.id, "user", &t.inbound_text)
                .unwrap();
        }

        let all = |f: &EpisodeFilter| {
            store
                .search_episodes_filtered("mutex", 10, f)
                .unwrap()
                .len()
        };
        assert_eq!(all(&EpisodeFilter::default()), 3);
        assert_eq!(
            all(&EpisodeFilter {
                model: Some("zai/glm-5.3".into()),
                ..EpisodeFilter::default()
            }),
            2
        );
        assert_eq!(
            all(&EpisodeFilter {
                model: Some("zai/glm-5.3".into()),
                after_ms: Some(5_000),
                ..EpisodeFilter::default()
            }),
            1
        );
        assert_eq!(
            all(&EpisodeFilter {
                status: Some("error".into()),
                ..EpisodeFilter::default()
            }),
            1
        );
        // The user split still fences a filtered search.
        assert_eq!(
            store
                .search_episodes_for_user_filtered(
                    "mutex",
                    10,
                    "alice",
                    &EpisodeFilter {
                        status: Some("error".into()),
                        ..EpisodeFilter::default()
                    }
                )
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            store
                .search_episodes_for_user_filtered(
                    "mutex",
                    10,
                    "alice",
                    &EpisodeFilter {
                        model: Some("anthropic/claude-sonnet-4-5".into()),
                        ..EpisodeFilter::default()
                    }
                )
                .unwrap()
                .len(),
            0,
            "a filtered search never returns another user's hits"
        );

        // No-FTS listing: the caller's own turns, newest first.
        let listed = store
            .list_turns_filtered(&EpisodeFilter::default(), Some("alice"), 10)
            .unwrap();
        assert_eq!(listed.len(), 2);
        assert_eq!(listed[0].snippet, "flaky mutex again");
        let errs = store
            .list_turns_filtered(
                &EpisodeFilter {
                    status: Some("error".into()),
                    ..EpisodeFilter::default()
                },
                None,
                10,
            )
            .unwrap();
        assert_eq!(errs.len(), 1);
        assert_eq!(errs[0].snippet, "flaky mutex again");
    }

    fn s_of(t: &Turn) -> SessionId {
        t.session.clone()
    }
}
