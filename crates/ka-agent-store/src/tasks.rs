//! Tasks (reminders, crons) and background run records.
//! Split from lib.rs (moves only).

use super::*;

impl Store {
    /// Tasks whose scheduled time has passed: reminders and crons (the
    /// scheduler branches on `kind`).
    pub fn due_tasks(&self, now_ms: i64) -> Result<Vec<ka_agent_core::Task>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT id, session, title, kind, status, read_only, scheduled_at_ms, payload, created_ms, updated_ms
             FROM tasks
             WHERE status = 'queued' AND scheduled_at_ms IS NOT NULL AND scheduled_at_ms <= ?1
             ORDER BY scheduled_at_ms",
        )?;
        let rows = stmt.query_map(params![now_ms], row_to_task)?;
        collect_rows(rows)
    }

    /// The earliest queued scheduled time, if any: the maintenance loop
    /// sleeps until it (bounded), instead of waking on a fixed interval.
    pub fn next_due_ms(&self) -> Result<Option<i64>> {
        let conn = self.lock();
        // MIN() always yields a row; NULL means nothing is scheduled.
        conn.query_row(
            "SELECT MIN(scheduled_at_ms) FROM tasks
             WHERE status = 'queued' AND scheduled_at_ms IS NOT NULL",
            [],
            |r| r.get::<_, Option<i64>>(0),
        )
        .map_err(|e| e.into())
    }

    /// Insert a task row.
    pub fn insert_task(&self, t: &ka_agent_core::Task) -> Result<()> {
        let conn = self.lock();
        conn.execute(
            "INSERT INTO tasks(id, session, title, kind, status, read_only, scheduled_at_ms, payload, created_ms, updated_ms, username)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            params![
                t.id.as_str(),
                t.session.as_ref().map(|s| s.as_str()),
                t.title,
                t.kind,
                task_status_str(&t.status),
                t.read_only as i64,
                t.scheduled_at_ms,
                t.payload,
                t.created_ms,
                t.updated_ms,
                task_username(t)
            ],
        )?;
        Ok(())
    }

    /// Update a task's status.
    pub fn set_task_status(&self, id: &ka_agent_core::TaskId, status: &str) -> Result<()> {
        let conn = self.lock();
        conn.execute(
            "UPDATE tasks SET status = ?2, updated_ms = ?3 WHERE id = ?1",
            params![id.as_str(), status, ka_agent_core::now_ms()],
        )?;
        Ok(())
    }

    /// Rewrite a task's payload in place (the cron's stored schedule
    /// metadata, e.g. a calendar digest retimed by `calendar.digest_time`).
    pub fn update_task_payload(&self, id: &ka_agent_core::TaskId, payload: &str) -> Result<()> {
        let conn = self.lock();
        conn.execute(
            "UPDATE tasks SET payload = ?2, updated_ms = ?3 WHERE id = ?1",
            params![id.as_str(), payload, ka_agent_core::now_ms()],
        )?;
        Ok(())
    }

    /// Update a task's schedule and status in one write (cron re-arming
    /// after a fire; pausing clears the schedule).
    pub fn update_task_schedule(
        &self,
        id: &ka_agent_core::TaskId,
        scheduled_at_ms: Option<i64>,
        status: &str,
    ) -> Result<()> {
        let conn = self.lock();
        conn.execute(
            "UPDATE tasks SET scheduled_at_ms = ?2, status = ?3, updated_ms = ?4 WHERE id = ?1",
            params![
                id.as_str(),
                scheduled_at_ms,
                status,
                ka_agent_core::now_ms()
            ],
        )?;
        Ok(())
    }

    /// Atomically claim a queued task for firing (`queued -> running`).
    /// `Ok(false)` when the row left `queued` first (a concurrent cancel
    /// or pause wins the race) — the caller must skip it.
    pub fn claim_task(&self, id: &ka_agent_core::TaskId) -> Result<bool> {
        let conn = self.lock();
        let changed = conn.execute(
            "UPDATE tasks SET status = 'running', updated_ms = ?2
             WHERE id = ?1 AND status = 'queued'",
            params![id.as_str(), ka_agent_core::now_ms()],
        )?;
        Ok(changed > 0)
    }

    /// Settle a claimed task (`running -> status`) only if it is still
    /// `running`. `Ok(false)` when a concurrent cancel/pause already took
    /// the row — the operator's decision wins.
    pub fn settle_task(&self, id: &ka_agent_core::TaskId, status: &str) -> Result<bool> {
        let conn = self.lock();
        let changed = conn.execute(
            "UPDATE tasks SET status = ?2, updated_ms = ?3
             WHERE id = ?1 AND status = 'running'",
            params![id.as_str(), status, ka_agent_core::now_ms()],
        )?;
        Ok(changed > 0)
    }

    /// Re-arm a claimed recurring task (`running -> queued` at the next
    /// occurrence). `Ok(false)` when a concurrent cancel/pause already took
    /// the row.
    pub fn rearm_task(&self, id: &ka_agent_core::TaskId, scheduled_at_ms: i64) -> Result<bool> {
        let conn = self.lock();
        let changed = conn.execute(
            "UPDATE tasks SET scheduled_at_ms = ?2, status = 'queued', updated_ms = ?3
             WHERE id = ?1 AND status = 'running'",
            params![id.as_str(), scheduled_at_ms, ka_agent_core::now_ms()],
        )?;
        Ok(changed > 0)
    }

    /// Restart reconciliation for tasks: a crash mid-fire leaves rows
    /// `running` forever; they become `interrupted` (visible as paused for
    /// crons, resumable via `cron_manage`). Returns the reconciled ids.
    pub fn reconcile_running_tasks(&self, now_ms: i64) -> Result<Vec<ka_agent_core::TaskId>> {
        let conn = self.lock();
        let mut stmt = conn.prepare("SELECT id FROM tasks WHERE status = 'running'")?;
        let ids = stmt
            .query_map([], |r| r.get::<_, String>(0).map(ka_agent_core::TaskId))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        drop(stmt);
        conn.execute(
            "UPDATE tasks SET status = 'interrupted', updated_ms = ?1 WHERE status = 'running'",
            params![now_ms],
        )?;
        Ok(ids)
    }

    /// Resume the crons `user disable` interrupted (account re-enabled).
    /// Returns how many tasks resumed.
    pub fn resume_user_crons(&self, username: &str) -> Result<usize> {
        let conn = self.lock();
        let n = conn.execute(
            "UPDATE tasks SET status = 'queued', updated_ms = ?2
             WHERE kind = 'cron' AND status = 'interrupted' AND username = ?1",
            params![username, ka_agent_core::now_ms()],
        )?;
        Ok(n)
    }

    /// Live cron jobs of one user (their partition; `admin` callers use
    /// [`Self::cron_tasks`]).
    pub fn cron_tasks_for_user(&self, username: &str) -> Result<Vec<ka_agent_core::Task>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT id, session, title, kind, status, read_only, scheduled_at_ms, payload, created_ms, updated_ms
             FROM tasks
             WHERE kind = 'cron' AND status IN ('queued','running','interrupted')
               AND username = ?1
             ORDER BY scheduled_at_ms",
        )?;
        let rows = stmt.query_map(params![username], row_to_task)?;
        collect_rows(rows)
    }

    /// Live cron tasks (queued, running or operator-paused), next fire
    /// first. Terminal crons are history, not schedule.
    pub fn cron_tasks(&self) -> Result<Vec<ka_agent_core::Task>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT id, session, title, kind, status, read_only, scheduled_at_ms, payload, created_ms, updated_ms
             FROM tasks
             WHERE kind = 'cron' AND status IN ('queued','running','interrupted')
             ORDER BY scheduled_at_ms",
        )?;
        let rows = stmt.query_map([], row_to_task)?;
        collect_rows(rows)
    }

    /// Live reminders and crons (queued, running or operator-paused), next
    /// fire first.
    pub fn scheduled_tasks(&self) -> Result<Vec<ka_agent_core::Task>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT id, session, title, kind, status, read_only, scheduled_at_ms, payload, created_ms, updated_ms
             FROM tasks
             WHERE kind IN ('reminder','cron') AND status IN ('queued','running','interrupted')
             ORDER BY scheduled_at_ms",
        )?;
        let rows = stmt.query_map([], row_to_task)?;
        collect_rows(rows)
    }

    /// One task by id, if it exists (any status).
    pub fn get_task(&self, id: &ka_agent_core::TaskId) -> Result<Option<ka_agent_core::Task>> {
        let conn = self.lock();
        let task = conn
            .query_row(
                "SELECT id, session, title, kind, status, read_only, scheduled_at_ms, payload, created_ms, updated_ms
                 FROM tasks WHERE id = ?1",
                params![id.as_str()],
                row_to_task,
            )
            .optional()?;
        Ok(task)
    }

    // ------------------------------------------------------------------
    // runs
    // ------------------------------------------------------------------

    /// Record the start of a background run.
    pub fn insert_run(&self, run: &RunRow) -> Result<()> {
        let conn = self.lock();
        conn.execute(
            "INSERT INTO runs(id, kind, ref_id, status, stop, error_class, error_message, cost_usd, tokens, started_ms, ended_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            params![
                run.id.as_str(),
                run.kind,
                run.ref_id,
                run.status,
                run.stop,
                run.error_class,
                run.error_message,
                run.cost_usd,
                run.tokens.map(|t| t as i64),
                run.started_ms,
                run.ended_ms
            ],
        )?;
        Ok(())
    }

    /// Fill in a run's terminal fields.
    #[allow(clippy::too_many_arguments)]
    pub fn finish_run(
        &self,
        id: &RunId,
        status: &str,
        stop: Option<&str>,
        error_class: Option<&str>,
        error_message: Option<&str>,
        cost_usd: Option<f64>,
        tokens: Option<u64>,
        ended_ms: i64,
    ) -> Result<()> {
        let conn = self.lock();
        let changed = conn.execute(
            "UPDATE runs SET status = ?2, stop = ?3, error_class = ?4, error_message = ?5,
                    cost_usd = ?6, tokens = ?7, ended_ms = ?8
             WHERE id = ?1",
            params![
                id.as_str(),
                status,
                stop,
                error_class,
                error_message,
                cost_usd,
                tokens.map(|t| t as i64),
                ended_ms
            ],
        )?;
        if changed == 0 {
            return Err(StoreError::NotFound(format!("run {}", id)));
        }
        Ok(())
    }

    /// Runs for one subject (a cron task id), newest first.
    pub fn runs_for(&self, ref_id: &str, limit: usize) -> Result<Vec<RunRow>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT id, kind, ref_id, status, stop, error_class, error_message, cost_usd, tokens, started_ms, ended_ms
             FROM runs WHERE ref_id = ?1 ORDER BY started_ms DESC LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![ref_id, limit as i64], row_to_run)?;
        collect_rows(rows)
    }

    /// Most recent runs, newest first.
    pub fn recent_runs(&self, limit: usize) -> Result<Vec<RunRow>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT id, kind, ref_id, status, stop, error_class, error_message, cost_usd, tokens, started_ms, ended_ms
             FROM runs ORDER BY started_ms DESC LIMIT ?1",
        )?;
        let rows = stmt.query_map(params![limit as i64], row_to_run)?;
        collect_rows(rows)
    }

    /// Live trigger tasks (queued, running or paused), armed first. A
    /// webhook trigger sits `queued` with no schedule while it waits for a
    /// delivery; terminal rows are history, not triggers.
    pub fn trigger_tasks(&self) -> Result<Vec<ka_agent_core::Task>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT id, session, title, kind, status, read_only, scheduled_at_ms, payload, created_ms, updated_ms
             FROM tasks
             WHERE kind = 'trigger' AND status IN ('queued','running','interrupted')
             ORDER BY scheduled_at_ms",
        )?;
        let rows = stmt.query_map([], row_to_task)?;
        collect_rows(rows)
    }

    /// Reserve a webhook event id (ADR-0005 §5): `Ok(true)` = first
    /// sighting, the delivery proceeds; `Ok(false)` = replay, the lane
    /// answers `dup` and arms nothing.
    pub fn record_trigger_event(&self, event_id: &str, trigger_id: &str) -> Result<bool> {
        let conn = self.lock();
        let inserted = conn.execute(
            "INSERT OR IGNORE INTO trigger_events(event_id, trigger_id, seen_ms) VALUES (?1, ?2, ?3)",
            params![event_id, trigger_id, ka_agent_core::now_ms()],
        )?;
        Ok(inserted > 0)
    }

    /// Park a fired webhook trigger: back to `queued` with no schedule
    /// (it waits for the next delivery to arm it again). Conditional on
    /// `running`, so an operator's cancel/pause wins the race.
    pub fn park_trigger(&self, id: &ka_agent_core::TaskId) -> Result<bool> {
        let conn = self.lock();
        let parked = conn.execute(
            "UPDATE tasks SET status = 'queued', scheduled_at_ms = NULL, updated_ms = ?2
             WHERE id = ?1 AND status = 'running'",
            params![id.as_str(), ka_agent_core::now_ms()],
        )?;
        Ok(parked > 0)
    }
}

/// The user a task belongs to: a cron's or trigger's creator (payload
/// `username`, else its role mapped through the user rule), reminders
/// always the operator.
pub fn task_username(t: &ka_agent_core::Task) -> String {
    match t.kind.as_str() {
        "cron" => t
            .payload
            .as_deref()
            .and_then(ka_agent_core::CronSpec::from_payload)
            .map(|c| match c.username {
                Some(u) => u,
                None => {
                    crate::user_for_role(c.role.as_deref().unwrap_or(ka_agent_core::ADMIN_ROLE))
                }
            })
            .unwrap_or_else(|| ka_agent_core::OPERATOR_USER.to_string()),
        "trigger" => t
            .payload
            .as_deref()
            .and_then(ka_agent_core::TriggerSpec::from_payload)
            .map(|s| match s.username {
                Some(u) => u,
                None => {
                    crate::user_for_role(s.role.as_deref().unwrap_or(ka_agent_core::ADMIN_ROLE))
                }
            })
            .unwrap_or_else(|| ka_agent_core::OPERATOR_USER.to_string()),
        _ => ka_agent_core::OPERATOR_USER.to_string(),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[test]
    fn runs_lifecycle() {
        let store = Store::open_in_memory().unwrap();
        let run = RunRow {
            id: RunId::generate(),
            kind: "task".into(),
            ref_id: Some("task-1".into()),
            status: "running".into(),
            stop: None,
            error_class: None,
            error_message: None,
            cost_usd: None,
            tokens: None,
            started_ms: 100,
            ended_ms: None,
        };
        store.insert_run(&run).unwrap();
        store
            .finish_run(
                &run.id,
                "succeeded",
                Some("done"),
                None,
                None,
                Some(0.25),
                Some(50),
                200,
            )
            .unwrap();
        let runs = store.recent_runs(5).unwrap();
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].status, "succeeded");
        assert_eq!(runs[0].tokens, Some(50));
    }

    #[test]
    fn trigger_events_dedupe_by_event_id() {
        let store = Store::open_in_memory().unwrap();
        assert!(store.record_trigger_event("ev-1", "task_1").unwrap());
        // Same event id, same or different trigger: a replay, not a new
        // delivery.
        assert!(!store.record_trigger_event("ev-1", "task_1").unwrap());
        assert!(!store.record_trigger_event("ev-1", "task_2").unwrap());
        assert!(store.record_trigger_event("ev-2", "task_1").unwrap());
    }

    #[test]
    fn trigger_task_rows_and_username() {
        let store = Store::open_in_memory().unwrap();
        let mut task = ka_agent_core::Task::new("hook github/label".into(), "trigger", false);
        task.payload = Some(
            ka_agent_core::TriggerSpec {
                kind: ka_agent_core::TriggerKind::Webhook,
                text: "triage".into(),
                source: "operator".into(),
                session_id: "sess_x".into(),
                role: Some("member".into()),
                username: Some("member".into()),
                tools: Vec::new(),
                error_streak: 0,
                recurrence_ms: None,
                at_local: None,
                weekdays: Vec::new(),
                month_day: None,
                gate: None,
                hook_path: Some("github/label".into()),
                secret: None,
                filter: None,
                max_body_bytes: None,
                preflight: false,
                memory_scope: ka_agent_core::TriggerMemoryScope::Automation,
            }
            .to_payload(),
        );
        store.insert_task(&task).unwrap();
        // The creator rides the username column (rbac.md's trigger row).
        assert_eq!(super::task_username(&task), "member");
        assert_eq!(store.trigger_tasks().unwrap().len(), 1);
        // Park is conditional: a running row parks back to queued with no
        // schedule; a queued row is untouched (it parks nothing).
        store.claim_task(&task.id).unwrap();
        assert!(store.park_trigger(&task.id).unwrap());
        let back = store.get_task(&task.id).unwrap().unwrap();
        assert_eq!(back.status, ka_agent_core::TaskStatus::Queued);
        assert!(back.scheduled_at_ms.is_none());
        // A cancelled trigger is never resurrected by a park.
        store.set_task_status(&task.id, "cancelled").unwrap();
        assert!(!store.park_trigger(&task.id).unwrap());
    }
}
