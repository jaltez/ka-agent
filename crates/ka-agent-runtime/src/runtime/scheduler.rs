// Scheduler: reminder/cron CRUD, firing, maintenance, briefings, the
// no-agent registry. Split from runtime.rs (moves only).
use super::*;

/// The end of the quiet window when `now` is inside it.
fn cfg_quiet_until(cfg: &DaemonConfig, now: &jiff::Zoned) -> Option<i64> {
    cfg.notify.quiet_until(now)
}

/// Next occurrence strictly after `now`, skipping missed occupations.
/// `None` on i64 overflow (an unarmable schedule, e.g. a hand-crafted
/// payload with an absurd interval).
fn next_occurrence(base_ms: i64, recurrence_ms: i64, now: i64) -> Option<i64> {
    let mut next = base_ms.checked_add(recurrence_ms)?;
    while next <= now {
        next = next.checked_add(recurrence_ms)?;
    }
    Some(next)
}

/// What `/remind rm` or `/cron rm` cancelled.
#[derive(Debug, Clone)]
pub enum CancelOutcome {
    Cancelled(String),
    NotFound,
    Ambiguous(Vec<String>),
}

impl Runtime {
    /// Schedule a reminder at an absolute time (parsing is the router's
    /// job: every surface shares one grammar).
    pub fn add_reminder_at(&self, at_ms: i64, text: &str) -> Result<ka_agent_core::TaskId, Error> {
        if text.trim().is_empty() {
            return Err(Error::Refused("a reminder needs a text".into()));
        }
        let mut task = ka_agent_core::Task::new(text.trim().to_string(), "reminder", false);
        task.scheduled_at_ms = Some(at_ms);
        self.sh.store.insert_task(&task)?;
        self.sh.scheduler_tick.notify_one();
        Ok(task.id)
    }

    /// Schedule a cron job: a `kind = "cron"` task carrying its
    /// [`ka_agent_core::CronSpec`] as payload, first firing at
    /// `first_at_ms`.
    pub fn add_cron(
        &self,
        spec: ka_agent_core::CronSpec,
        first_at_ms: i64,
    ) -> Result<ka_agent_core::TaskId, Error> {
        let mut task = ka_agent_core::Task::new(spec.text.clone(), "cron", false);
        task.scheduled_at_ms = Some(first_at_ms);
        task.payload = Some(spec.to_payload());
        self.sh.store.insert_task(&task)?;
        self.sh.scheduler_tick.notify_one();
        Ok(task.id)
    }

    /// Live cron jobs (queued, running or operator-paused), next fire first.
    pub fn list_crons(&self) -> Result<Vec<ka_agent_core::Task>, Error> {
        Ok(self.sh.store.cron_tasks()?)
    }

    /// Live reminders and crons, next fire first (`/reminders`).
    pub fn scheduled_tasks(&self) -> Result<Vec<ka_agent_core::Task>, Error> {
        Ok(self.sh.store.scheduled_tasks()?)
    }

    /// Cancel any scheduled task (reminder or cron) by id prefix.
    pub fn cancel_scheduled(&self, id_prefix: &str) -> Result<CancelOutcome, Error> {
        if id_prefix.trim().is_empty() {
            return Ok(CancelOutcome::NotFound);
        }
        let matches: Vec<_> = self
            .sh
            .store
            .scheduled_tasks()?
            .into_iter()
            .filter(|t| ka_agent_core::ids::id_matches(t.id.as_str(), id_prefix.trim()))
            .collect();
        match matches.as_slice() {
            [] => Ok(CancelOutcome::NotFound),
            [one] => {
                self.sh.store.set_task_status(&one.id, "cancelled")?;
                Ok(CancelOutcome::Cancelled(one.title.clone()))
            }
            many => Ok(CancelOutcome::Ambiguous(
                many.iter()
                    .map(|t| ka_agent_core::ids::short_id(t.id.as_str()).to_string())
                    .collect(),
            )),
        }
    }

    /// Fire due tasks (reminders and crons); called from the scheduler
    /// timer. Each item is claimed (`queued -> running`) before it acts and
    /// settled after; a concurrent cancel/pause wins both races (claim and
    /// settle are conditional), and one failing item never stops the batch.
    pub async fn fire_due_tasks(&self) -> Result<Vec<String>, Error> {
        let now = ka_agent_core::now_ms();
        let due = self.sh.store.due_tasks(now)?;
        let mut fired = Vec::new();
        // SEED.md §7.2: a fire that sat overdue fires late (and says so)
        // within the grace hour; beyond it, one-shots are recorded `missed`
        // and recurring jobs skip the missed occupation and re-arm.
        const LATE_GRACE_MS: i64 = 3_600_000;
        const LATE_MARK_MS: i64 = 60_000;
        let now_zoned = jiff::Timestamp::now().to_zoned(self.cfg().time_zone());
        // Anchored recurrence (`at_local`): the next occurrence is the next
        // local wall-clock hit (weekday-optional), DST-correct — not a
        // fixed-interval hop from the previous fire.
        let next_for = |spec: &ka_agent_core::CronSpec, now: i64| -> Option<i64> {
            let Some(at) = spec.at_local.as_deref() else {
                return next_occurrence(
                    now,
                    spec.recurrence_ms.unwrap_or(0),
                    ka_agent_core::now_ms(),
                );
            };
            let (hh, mm) = ka_agent_core::when::parse_hhmm(at)?;
            Some(ka_agent_core::when::next_local(
                hh,
                mm,
                &spec.day_rule(),
                &now_zoned,
            ))
        };
        let late_by = |task: &ka_agent_core::Task| now - task.scheduled_at_ms.unwrap_or(now);
        let late_suffix = |late_by: i64| {
            if late_by > LATE_MARK_MS {
                format!(" (late by {})", ka_agent_core::fmt_interval(late_by))
            } else {
                String::new()
            }
        };
        // One `runs` row per cron fire (`ka-agent cron runs <id>`): what
        // happened, not just that the row moved on.
        let record = |task: &ka_agent_core::Task, status: &str, error: Option<&str>| {
            let now = ka_agent_core::now_ms();
            let run = ka_agent_store::RunRow {
                id: ka_agent_core::RunId::generate(),
                kind: "cron".into(),
                ref_id: Some(task.id.as_str().to_string()),
                status: status.into(),
                stop: None,
                error_class: None,
                error_message: error.map(str::to_string),
                cost_usd: None,
                tokens: None,
                started_ms: now,
                ended_ms: Some(now),
            };
            let _ = self.sh.store.insert_run(&run);
        };
        for task in due {
            // Claim: the snapshot is stale the moment another surface
            // (cancel, pause, remove) touched the row. Skip what we lost.
            if !self.sh.store.claim_task(&task.id).unwrap_or(false) {
                continue;
            }
            match task.kind.as_str() {
                "reminder" => {
                    // A reminder is a notification, not a prompt: firing
                    // it must not spend a model turn. Work that needs the
                    // engine is a cron job with `mode = agent`, chosen
                    // explicitly by the operator.
                    let title = task.title.clone();
                    let this_late_by = late_by(&task);
                    if this_late_by > LATE_GRACE_MS {
                        self.sh.emit(AgentEvent::Reminder {
                            id: task.id.to_string(),
                            text: format!(
                                "missed: {title} (overdue by {})",
                                ka_agent_core::fmt_interval(this_late_by)
                            ),
                        });
                        let _ = self.sh.store.settle_task(&task.id, "missed");
                        continue;
                    }
                    // Structured so every surface can render it its own
                    // way (the chat shows ⏰, phones a notification).
                    self.sh.emit(AgentEvent::Reminder {
                        id: task.id.to_string(),
                        text: format!("{title}{}", late_suffix(this_late_by)),
                    });
                    let _ = self.sh.store.settle_task(&task.id, "succeeded");
                    fired.push(title);
                }
                "cron" => {
                    let Some(spec) = ka_agent_core::CronSpec::from_payload(
                        task.payload.as_deref().unwrap_or(""),
                    ) else {
                        let _ = self.sh.store.settle_task(&task.id, "failed");
                        continue;
                    };
                    let this_late_by = late_by(&task);
                    let recurring = spec.recurrence_ms.is_some_and(|rec| rec > 0);
                    if this_late_by > LATE_GRACE_MS {
                        if recurring {
                            // A recurring job outlives one missed occupation:
                            // skip it and re-arm strictly after now.
                            let note = match next_for(&spec, task.scheduled_at_ms.unwrap_or(now)) {
                                Some(next) => {
                                    let _ = self.sh.store.rearm_task(&task.id, next);
                                    format!(
                                        "cron '{}' skipped a missed occurrence (overdue by {}); re-armed",
                                        spec.text,
                                        ka_agent_core::fmt_interval(this_late_by)
                                    )
                                }
                                None => {
                                    let _ = self.sh.store.settle_task(&task.id, "failed");
                                    "cron re-arm overflow".to_string()
                                }
                            };
                            self.sh.emit(AgentEvent::Notice { text: note });
                            record(&task, "missed", None);
                            continue;
                        }
                        self.sh.emit(AgentEvent::Notice {
                            text: format!(
                                "cron '{}' missed (overdue by {})",
                                spec.text,
                                ka_agent_core::fmt_interval(this_late_by)
                            ),
                        });
                        let _ = self.sh.store.settle_task(&task.id, "missed");
                        record(&task, "missed", None);
                        continue;
                    }
                    // Quiet hours: a scheduled prompt that falls due inside
                    // the window waits for its end (nothing runs for nobody
                    // to read); reminders above are never held.
                    if let Some(until) = cfg_quiet_until(&self.cfg(), &now_zoned) {
                        let _ = self.sh.store.rearm_task(&task.id, until);
                        record(&task, "deferred", Some("quiet hours"));
                        continue;
                    }
                    // Preflight (agent jobs): never spend — or pretend to
                    // have spent — a turn that cannot happen. No model/key:
                    // pause the job (it cannot succeed until the operator
                    // acts). Budget reached: skip this occurrence (the
                    // window rolls tomorrow).
                    if spec.mode == ka_agent_core::CronMode::Agent {
                        if let Err(why) = self.preflight() {
                            let _ = self.sh.store.set_task_status(&task.id, "interrupted");
                            self.sh.emit(AgentEvent::Notice {
                                text: format!(
                                    "cron '{}' paused: {why} — then `ka-agent cron resume {}`",
                                    spec.text,
                                    ka_agent_core::ids::short_id(task.id.as_str())
                                ),
                            });
                            record(&task, "blocked", Some(&why));
                            continue;
                        }
                        let exceeded =
                            self.lock().budget.verdict() == ka_agent_core::BudgetVerdict::Exceeded;
                        if exceeded {
                            let next = if recurring {
                                next_for(&spec, task.scheduled_at_ms.unwrap_or(now))
                            } else {
                                None
                            };
                            match next {
                                Some(next) => {
                                    let _ = self.sh.store.rearm_task(&task.id, next);
                                }
                                None => {
                                    let _ = self.sh.store.settle_task(&task.id, "failed");
                                }
                            }
                            self.sh.emit(AgentEvent::Notice {
                                text: format!(
                                    "cron '{}' skipped: today's budget is reached",
                                    spec.text
                                ),
                            });
                            record(&task, "skipped", Some("daily budget reached"));
                            continue;
                        }
                    }
                    let ok = match &spec.mode {
                        ka_agent_core::CronMode::Agent => {
                            let (prompt, label) = if spec.source == crate::briefing::SOURCE {
                                (
                                    format!(
                                        "{}\n\n{}",
                                        crate::briefing::PROMPT,
                                        self.briefing_context()
                                    ),
                                    "☀ daily briefing".to_string(),
                                )
                            } else {
                                (spec.text.clone(), format!("cron: {}", spec.text))
                            };
                            self.sh.emit(AgentEvent::Notice {
                                text: format!("{label}{}", late_suffix(this_late_by)),
                            });
                            let turn_source = if spec.source == crate::briefing::SOURCE {
                                crate::briefing::SOURCE
                            } else {
                                "cron"
                            };
                            match self.handle_message(&prompt, turn_source).await {
                                Ok(()) => true,
                                Err(e) => {
                                    self.sh.emit(AgentEvent::Error {
                                        class: "cron".into(),
                                        message: format!("{}: {e}", task.id.as_str()),
                                    });
                                    false
                                }
                            }
                        }
                        ka_agent_core::CronMode::NoAgent(kind) => {
                            match self.run_no_agent(kind, &spec.text) {
                                Ok(out) => {
                                    self.sh.emit(AgentEvent::Notice { text: out });
                                    true
                                }
                                Err(e) => {
                                    self.sh.emit(AgentEvent::Error {
                                        class: "cron".into(),
                                        message: format!("{}: {e}", task.id.as_str()),
                                    });
                                    false
                                }
                            }
                        }
                    };
                    record(&task, if ok { "fired" } else { "failed" }, None);
                    if !ok {
                        // Failed jobs are not re-armed: the failure is
                        // visible on the surface and the row stays `failed`.
                        let _ = self.sh.store.settle_task(&task.id, "failed");
                        continue;
                    }
                    match spec.recurrence_ms {
                        Some(rec) if rec > 0 => {
                            match next_for(&spec, task.scheduled_at_ms.unwrap_or(now)) {
                                // Re-arm only if the row is still `running` (a
                                // cancel mid-flight wins).
                                Some(next) => {
                                    let _ = self.sh.store.rearm_task(&task.id, next);
                                }
                                // Overflowing schedule: unarmable, fail visibly.
                                None => {
                                    self.sh.emit(AgentEvent::Error {
                                        class: "cron".into(),
                                        message: format!(
                                            "{}: recurrence overflow",
                                            task.id.as_str()
                                        ),
                                    });
                                    let _ = self.sh.store.settle_task(&task.id, "failed");
                                    continue;
                                }
                            }
                        }
                        Some(_) => {
                            let _ = self.sh.store.settle_task(&task.id, "failed");
                            continue;
                        }
                        None => {
                            let _ = self.sh.store.settle_task(&task.id, "succeeded");
                        }
                    }
                    fired.push(spec.text);
                }
                other => {
                    self.sh.emit(AgentEvent::Error {
                        class: "cron".into(),
                        message: format!("unknown scheduled kind {other:?}"),
                    });
                    let _ = self.sh.store.settle_task(&task.id, "failed");
                }
            }
        }
        Ok(fired)
    }

    /// The maintenance loop: one wake for the approval-expiry sweep and
    /// the scheduler. Sleeps until the next due task when one is queued
    /// (bounded by `every`), otherwise a plain `every` interval — an idle
    /// daemon ticks once per interval, not per concern. `scheduler_tick`
    /// interrupts the sleep so an in-process `/remind` or `/cron add`
    /// recomputes the deadline immediately; engine-side `cron_manage`
    /// writes the store from another process and waits at most `every`.
    pub async fn run_maintenance(&self, every: Duration) {
        loop {
            if let Err(e) = self.expire_approvals().await {
                self.sh.emit(AgentEvent::Error {
                    class: "internal".into(),
                    message: format!("expiry sweep: {e}"),
                });
            }
            if let Err(e) = self.fire_due_tasks().await {
                self.sh.emit(AgentEvent::Error {
                    class: "internal".into(),
                    message: format!("scheduler: {e}"),
                });
            }
            self.reap_idle_worker().await;
            self.watch_config_file().await;
            self.watch_persona().await;
            // Bounded launchd/redirect logs (journal systems rotate alone).
            let _ = self.sh.paths.rotate_log(5 * 1024 * 1024);
            if let Err(e) = self.announce_memory_proposals() {
                self.sh.emit(AgentEvent::Error {
                    class: "internal".into(),
                    message: format!("memory proposals: {e}"),
                });
            }
            self.maybe_prune().await;
            let wait = match self.sh.store.next_due_ms() {
                Ok(Some(next)) => {
                    let now = ka_agent_core::now_ms();
                    if next <= now {
                        Duration::ZERO
                    } else {
                        Duration::from_millis((next - now) as u64 + 100)
                    }
                }
                _ => every,
            };
            // Floor guards against a spin when a due task cannot be
            // claimed; the cap bounds cross-process scheduling latency.
            let wait = wait.min(every).max(Duration::from_millis(250));
            tokio::select! {
                _ = tokio::time::sleep(wait) => {}
                _ = self.sh.scheduler_tick.notified() => {}
            }
        }
    }

    /// Store retention: prune terminal rows every 6 h (meta-keyed so a
    /// restarted daemon does not re-prune immediately).
    async fn maybe_prune(&self) {
        const INTERVAL_MS: i64 = 6 * 60 * 60 * 1000;
        let now = ka_agent_core::now_ms();
        let due = self
            .sh
            .store
            .meta_get("prune.last_ms")
            .ok()
            .flatten()
            .and_then(|v| v.parse::<i64>().ok())
            .is_none_or(|last| now - last >= INTERVAL_MS);
        if !due {
            return;
        }
        match self.sh.store.prune(now) {
            Ok(report) => {
                let _ = self.sh.store.meta_set("prune.last_ms", &now.to_string());
                let pruned = report.runs
                    + report.approvals
                    + report.intents
                    + report.memory_candidates
                    + report.queue
                    + report.outbox
                    + report.tasks;
                if pruned > 0 {
                    self.sh.emit(AgentEvent::Notice {
                        text: format!(
                            "state pruned: {pruned} old row(s) (runs, approvals, queue, outbox)"
                        ),
                    });
                }
            }
            Err(e) => {
                self.sh.emit(AgentEvent::Error {
                    class: "internal".into(),
                    message: format!("prune: {e}"),
                });
            }
        }
    }

    /// What the briefing prompt needs and the model cannot see: today's
    /// reminders, what waits for the operator, calendar digests.
    pub(super) fn briefing_context(&self) -> String {
        let now = self.now_zoned();
        let end_of_day = now
            .date()
            .tomorrow()
            .ok()
            .and_then(|d| d.to_zoned(now.time_zone().clone()).ok())
            .map(|z| z.timestamp().as_millisecond())
            .unwrap_or(i64::MAX);
        let mut lines = vec![format!(
            "Context (now {}):",
            now.strftime("%A %Y-%m-%d %H:%M")
        )];
        let tasks = self.sh.store.scheduled_tasks().unwrap_or_default();
        let today: Vec<String> = tasks
            .iter()
            .filter(|t| t.kind == "reminder")
            .filter_map(|t| t.scheduled_at_ms.map(|at| (at, t)))
            .filter(|(at, _)| *at < end_of_day)
            .map(|(at, t)| {
                format!(
                    "- reminder {} — {}",
                    ka_agent_core::when::describe(at, &now),
                    t.title
                )
            })
            .collect();
        if today.is_empty() {
            lines.push("- no reminders today".into());
        } else {
            lines.extend(today);
        }
        let approvals = self
            .sh
            .store
            .pending_approvals(&self.session_id())
            .map(|a| a.len())
            .unwrap_or(0);
        let review = self
            .sh
            .store
            .pending_candidates_full()
            .map(|c| c.len())
            .unwrap_or(0);
        lines.push(format!(
            "- waiting for the operator: {approvals} approval(s), {review} memory proposal(s)"
        ));
        for t in &tasks {
            if let Some(spec) = t
                .payload
                .as_deref()
                .and_then(ka_agent_core::CronSpec::from_payload)
                && let ka_agent_core::CronMode::NoAgent(kind) = &spec.mode
                && kind == "calendar"
                && let Ok(digest) = self.run_no_agent(kind, &spec.text)
            {
                lines.push(format!("- calendar:\n{digest}"));
            }
        }
        lines.join("\n")
    }

    /// Register a no-agent cron handler by kind (e.g. "calendar"). The app
    /// binary does this at startup; handlers run on the scheduler tick and
    /// never touch the engine.
    pub fn set_no_agent_handler(&self, kind: &str, f: NoAgentFn) {
        let mut reg = self.sh.no_agent.lock().unwrap_or_else(|e| e.into_inner());
        reg.insert(kind.to_string(), f);
    }

    /// Dispatch a no-agent cron handler by kind.
    pub fn run_no_agent(&self, kind: &str, arg: &str) -> Result<String, Error> {
        let handler = {
            let reg = self.sh.no_agent.lock().unwrap_or_else(|e| e.into_inner());
            reg.get(kind).cloned()
        };
        match handler {
            Some(f) => f(arg).map_err(Error::Refused),
            None => Err(Error::Refused(format!("no handler for kind {kind:?}"))),
        }
    }
}
