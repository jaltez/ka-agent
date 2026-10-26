// Inbound handling: message routing, preflight, submit, the durable queue.
// Split from runtime.rs (moves only).
use super::worker::rt_on_engine_closed;
use super::*;

/// Who started the turn being submitted, for the bridge's loop guard.
fn publish_turn_source(store: &Store, source: &str) {
    let _ = store.meta_set(ka_agent_core::layout::TURN_SOURCE_META_KEY, source);
}

/// Submit the next queued message, if any. Returns true when a new cycle is
/// running (session stays `running`).
/// The per-turn context line the model sees first: local clock, weekday,
/// zone and channel; scheduled turns name their origin. `turns.inbound_text`
/// and the FTS index keep the raw text.
fn context_line(sh: &Shared, source: &str) -> String {
    let now = jiff::Timestamp::now().to_zoned(cfg_of(sh).time_zone());
    ctx_line(&now, source)
}

/// The pure form (unit-tested): one `<ctx .../>` line for `now` + source.
fn ctx_line(now: &jiff::Zoned, source: &str) -> String {
    let tz = now.time_zone().iana_name().unwrap_or("UTC").to_string();
    let day = match now.weekday() {
        jiff::civil::Weekday::Monday => "Mon",
        jiff::civil::Weekday::Tuesday => "Tue",
        jiff::civil::Weekday::Wednesday => "Wed",
        jiff::civil::Weekday::Thursday => "Thu",
        jiff::civil::Weekday::Friday => "Fri",
        jiff::civil::Weekday::Saturday => "Sat",
        jiff::civil::Weekday::Sunday => "Sun",
    };
    // console -> terminal; "-bg" background asks speak as their channel;
    // a cron:<id> source names the task in `scheduled`.
    let (via, scheduled) = if let Some(rest) = source.strip_prefix("cron:") {
        ("cron", Some(format!("cron:{rest}")))
    } else if source == "cron" {
        ("cron", Some("cron".to_string()))
    } else if source == "briefing" {
        ("briefing", Some("briefing".to_string()))
    } else {
        let bare = source.strip_suffix("-bg").unwrap_or(source);
        let bare = bare.split(':').next().unwrap_or(bare);
        let via = if bare == "console" { "terminal" } else { bare };
        (via, None)
    };
    let scheduled = scheduled
        .map(|s| format!(" scheduled=\"{s}\""))
        .unwrap_or_default();
    format!(
        "<ctx now=\"{}\" day=\"{day}\" tz=\"{tz}\" via=\"{via}\"{scheduled}/>",
        now.strftime("%Y-%m-%dT%H:%M:%S%:z")
    )
}

pub(super) async fn drain_queue(sh: &Arc<Shared>) -> bool {
    let sid = {
        sh.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .session
            .id
            .clone()
    };
    let Some(item) = sh.store.next_queued(&sid).ok().flatten() else {
        return false;
    };
    let port = {
        let g = sh.inner.lock().unwrap_or_else(|e| e.into_inner());
        g.live.as_ref().map(|l| Arc::clone(&l.port))
    };
    let Some(port) = port else {
        return false;
    };
    // Queued messages cross channels too: the transcript sees them the
    // same as directly-submitted ones (drain bypasses handle_message).
    sh.emit(AgentEvent::Inbound {
        source: item.source.clone(),
        text: item.text.clone(),
    });
    publish_turn_source(&sh.store, &item.source);
    let mut turn = Turn::new(sid.clone(), item.text.clone());
    if let Err(e) = sh.store.insert_turn(&turn) {
        sh.emit(AgentEvent::Error {
            class: "internal".into(),
            message: format!("queue drain: {e}"),
        });
        return false;
    }
    if turn.transition(TurnStatus::Submitted).is_err() {
        return false;
    }
    sh.store.update_turn(&turn).ok();
    sh.store.mark_turned(&item.id, &turn.id).ok();
    // Claim the turn *before* the submit handshake, exactly like
    // `submit_text_with`: the engine can stream and finish while we are
    // still awaiting `port.submit`, and a late claim attributes the finish
    // to the previous turn (overwriting its chunks and cost) and strands
    // this one `running`.
    {
        let mut g = sh.inner.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(live) = g.live.as_mut() {
            live.current_turn = Some(turn.id.clone());
            live.current_turn_source = Some(item.source.clone());
            live.turn_started_at = Some(Instant::now());
            live.live_text.clear();
        }
    }
    let wire = format!("{}\n{}", context_line(sh, &item.source), item.text);
    // Background turns may run on the background model.
    let background = item.source.starts_with("cron")
        || item.source == "briefing"
        || item.source.ends_with("-bg");
    let model = background
        .then(|| cfg_of(sh).background_model.clone())
        .flatten();
    match port
        .submit_with_model(wire, Vec::new(), Vec::new(), model)
        .await
    {
        Ok(()) => {
            turn.transition(TurnStatus::Running)
                .map_err(|t| illegal_turn(turn.status, t))
                .ok();
            sh.store.update_turn(&turn).ok();
            true
        }
        Err(_) => {
            turn.transition(TurnStatus::Interrupted)
                .map_err(|t| illegal_turn(turn.status, t))
                .ok();
            sh.store.update_turn(&turn).ok();
            false
        }
    }
}

impl Runtime {
    /// Inbound message: submit now when idle, otherwise enqueue durably
    /// (gap G4 — ka drops mid-cycle prompts, HTTP 200 notwithstanding).
    pub async fn handle_message(&self, text: &str, source: &str) -> Result<(), Error> {
        self.sh.emit(AgentEvent::Inbound {
            source: source.to_string(),
            text: text.to_string(),
        });
        let mut submit_head = false;
        let submit_now = {
            let mut g = self.lock();
            match g.session.status {
                SessionStatus::Idle => {
                    // FIFO across restarts: the worker spawns lazily, so a
                    // durable queue left by the previous process waits for
                    // exactly this message — which must not overtake it.
                    // Enqueue behind it and submit the head instead.
                    if self.sh.store.queue_len(&g.session.id).unwrap_or(0) > 0 {
                        let item = QueueItem::new(g.session.id.clone(), text.to_string(), source);
                        self.sh.store.enqueue(&item)?;
                        submit_head = true;
                    }
                    let from = g.session.status;
                    g.session
                        .transition(SessionStatus::Running)
                        .map_err(|t| illegal_session(from, t))?;
                    true
                }
                SessionStatus::Running
                | SessionStatus::AwaitingApproval
                | SessionStatus::Stopping => false,
                SessionStatus::Paused => {
                    // A budget pause lifts itself when the day window rolls
                    // (the verdict is recomputed from the recorded window);
                    // an auth pause needs /resume, which is why the note
                    // says so honestly. `g` is already held — relocking the
                    // non-reentrant mutex here deadlocked every message
                    // after a pause.
                    if g.budget.verdict() == ka_agent_core::BudgetVerdict::Exceeded {
                        self.sh.emit(AgentEvent::Notice {
                            text: "paused: budget reached for this window; use /resume to override or wait for the window to roll"
                                .into(),
                        });
                        false
                    } else {
                        // Paused→Running is not a legal edge; go through
                        // idle. Both writes are persisted: the store checks
                        // transitions against the stored row, and skipping
                        // the idle write would make the running write
                        // illegal.
                        g.session
                            .transition(SessionStatus::Idle)
                            .map_err(|t| illegal_session(SessionStatus::Paused, t))?;
                        g.session
                            .transition(SessionStatus::Running)
                            .map_err(|t| illegal_session(SessionStatus::Idle, t))?;
                        drop(g);
                        self.persist_session_status(SessionStatus::Idle);
                        true
                    }
                }
                SessionStatus::Degraded => {
                    self.sh.emit(AgentEvent::Notice {
                        text: "⚠ degraded — the worker keeps crashing; /new starts a fresh session"
                            .into(),
                    });
                    false
                }
            }
        };
        if submit_now {
            // Persist immediately: the store is the durable source of truth
            // for recovery, and a row left at `idle` makes every later status
            // write look like an illegal transition.
            self.persist_session_status(SessionStatus::Running);
        }
        if !submit_now {
            match self.session_status() {
                SessionStatus::Running
                | SessionStatus::AwaitingApproval
                | SessionStatus::Stopping => {
                    // `busy_messages = "steer"`: an operator message lands
                    // in the running reply at the next tool-batch boundary.
                    // Scheduled turns stay uninterrupted, and a session
                    // with no live turn (awaiting a crash recovery) queues.
                    let live = self.live_port();
                    let running = self.current_turn_source();
                    let steerable = self.cfg().busy_messages == "steer"
                        && live.is_some()
                        && !running.as_deref().is_some_and(|s| {
                            s == "cron" || s.starts_with("cron:") || s == "briefing"
                        });
                    if steerable
                        && let Some(port) = live
                        && port.steer(text.to_string()).await.is_ok()
                    {
                        // Record it on the turn (the durable transcript)
                        // and in the live stream the surfaces show.
                        let turn_id = self
                            .lock()
                            .live
                            .as_ref()
                            .and_then(|l| l.current_turn.clone());
                        if let Some(id) = turn_id {
                            let _ = self.sh.store.append_turn_inbound(
                                &id,
                                &format!("\n\n[added while working] {text}"),
                            );
                        }
                        if let Some(live) = self.lock().live.as_mut() {
                            live.live_text
                                .push_str(&format!("\n\n[added while working] {text}"));
                        }
                        self.sh.emit(AgentEvent::Note {
                            text: "steered into the running reply".into(),
                        });
                        return Ok(());
                    }
                    let item = QueueItem::new(self.session_id(), text.to_string(), source);
                    self.sh.store.enqueue(&item)?;
                    let n = self.sh.store.queue_len(&self.session_id())?;
                    self.sh.emit(AgentEvent::Queue { depth: n as u32 });
                    self.sh.emit(AgentEvent::Notice {
                        text: format!("queued (#{n})"),
                    });
                }
                _ => {}
            }
            return Ok(());
        }
        if submit_head {
            // (drain_queue publishes the head item's own source.)
            // The head item goes through the same handshake as a direct
            // submit: the worker is spawned lazily, so it must exist before
            // the drain (drain_queue only ever uses a live port).
            if let Err(e) = self.ensure_worker().await {
                let mut g = self.lock();
                if g.session.status == SessionStatus::Running {
                    g.session.transition(SessionStatus::Idle).ok();
                }
                drop(g);
                self.persist_session_status(SessionStatus::Idle);
                return Err(e);
            }
            drain_queue(&self.sh).await;
            return Ok(());
        }
        publish_turn_source(&self.sh.store, source);
        match self.submit_text_with(text, Vec::new(), Some(source)).await {
            Ok(()) => Ok(()),
            Err(e) => {
                // Never strand the session in `running`: the queue would
                // swallow every later message (single-flight).
                let mut g = self.lock();
                if g.session.status == SessionStatus::Running {
                    g.session.transition(SessionStatus::Idle).ok();
                }
                drop(g);
                self.persist_session_status(SessionStatus::Idle);
                Err(e)
            }
        }
    }

    /// `/skill <name> <text>`: submit one turn with that skill's full body
    /// injected (SEED.md §2.6, §9.1). Fails closed: the skill must be
    /// provisioned in the engine home, and the session must be idle — a
    /// skill turn is never queued (the queue carries plain text only).
    pub async fn handle_skill(&self, name: &str, text: &str, source: &str) -> Result<(), Error> {
        let skills = crate::skills::Skills::new(&self.sh.paths.engine_home());
        if !skills.is_enabled(name)? {
            return Err(Error::Refused(format!(
                "skill {name:?} is not enabled — `ka-agent skills enable {name}`"
            )));
        }
        if text.trim().is_empty() {
            return Err(Error::Refused("usage: /skill <name> <text>".into()));
        }
        {
            let mut g = self.lock();
            if g.session.status != SessionStatus::Idle {
                return Err(Error::Refused(
                    "session busy — /skill runs when idle; nothing was queued".into(),
                ));
            }
            let from = g.session.status;
            g.session
                .transition(SessionStatus::Running)
                .map_err(|t| illegal_session(from, t))?;
        }
        self.persist_session_status(SessionStatus::Running);
        match self
            .submit_text_with(text, vec![name.to_string()], Some(source))
            .await
        {
            Ok(()) => Ok(()),
            Err(e) => {
                let mut g = self.lock();
                if g.session.status == SessionStatus::Running {
                    g.session.transition(SessionStatus::Idle).ok();
                }
                drop(g);
                self.persist_session_status(SessionStatus::Idle);
                Err(e)
            }
        }
    }

    /// Refuse a message that cannot possibly work, before any state moves:
    /// no model configured, or the model's provider key missing. The
    /// refusal is the operator's answer; the session returns to idle and
    /// nothing is submitted or recorded.
    pub(super) fn preflight(&self) -> Result<(), String> {
        // Scripted speakers (e2e) legitimately run without model or keys.
        if self.sh.port.scenario_override.is_some() {
            return Ok(());
        }
        let cfg = self.cfg();
        if cfg.model.is_none() && cfg.ka.engine_layer.is_none() {
            return Err(
                "no model configured — run `ka-agent setup` (or /model provider/model)".into(),
            );
        }
        if let Some(model) = &cfg.model {
            // Validate once per model string; re-read the .env file only
            // when its mtime moved (preflight runs on every submit).
            let sel = {
                let mut cache = self
                    .sh
                    .selector_cache
                    .lock()
                    .unwrap_or_else(|e| e.into_inner());
                if let Some((cached_model, sel)) = cache.as_ref() {
                    if cached_model == model {
                        sel.clone()
                    } else {
                        let sel = ka_agent_port::catalog::validate_selector(model)?;
                        *cache = Some((model.clone(), sel.clone()));
                        sel
                    }
                } else {
                    let sel = ka_agent_port::catalog::validate_selector(model)?;
                    *cache = Some((model.clone(), sel.clone()));
                    sel
                }
            };
            let env_path = ka_agent_port::catalog::engine_env_path(&self.sh.paths.engine_home());
            let mtime = std::fs::metadata(&env_path)
                .and_then(|m| m.modified())
                .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
            let env = {
                let mut cache = self.sh.env_cache.lock().unwrap_or_else(|e| e.into_inner());
                if let Some((cached_mtime, env)) = cache.as_ref() {
                    if *cached_mtime == mtime {
                        env.clone()
                    } else {
                        let env = ka_agent_port::catalog::read_env_file(&env_path);
                        *cache = Some((mtime, env.clone()));
                        env
                    }
                } else {
                    let env = ka_agent_port::catalog::read_env_file(&env_path);
                    *cache = Some((mtime, env.clone()));
                    env
                }
            };
            if let ka_agent_port::catalog::KeyState::Missing(name) =
                ka_agent_port::catalog::key_state(&sel, &env)
            {
                return Err(format!(
                    "{model} needs {name} — on your computer run: ka-agent key set {name}"
                ));
            }
        }
        Ok(())
    }

    /// Submit with per-turn skills (`Command::Prompt.skills`: the engine
    /// injects the full SKILL.md bodies for these names into this turn).
    async fn submit_text_with(
        &self,
        text: &str,
        skills: Vec<String>,
        source: Option<&str>,
    ) -> Result<(), Error> {
        if let Err(msg) = self.preflight() {
            self.sh.emit(AgentEvent::Notice { text: msg });
            let mut g = self.lock();
            if g.session.status == SessionStatus::Running {
                g.session.transition(SessionStatus::Idle).ok();
            }
            drop(g);
            self.persist_session_status(SessionStatus::Idle);
            return Ok(());
        }
        if let Err(e) = self.ensure_worker().await {
            // A dead worker at submit time behaves like an interruption:
            // surface it, never auto-retry (SEED.md §3.8).
            self.sh.emit(AgentEvent::Error {
                class: "internal".into(),
                message: format!("worker start failed: {e}"),
            });
            let mut g = self.lock();
            let from = g.session.status;
            g.session
                .transition(SessionStatus::Idle)
                .map_err(|t| illegal_session(from, t))?;
            return Ok(());
        }
        let (sid, port) = {
            let g = self.lock();
            let Some(live) = g.live.as_ref() else {
                return Ok(());
            };
            (g.session.id.clone(), Arc::clone(&live.port))
        };
        // Record the turn BEFORE submitting (gap G8: ka persists per cycle,
        // so Ka Agent's record is the durable one).
        let mut turn = Turn::new(sid, text.to_string());
        self.sh.store.insert_turn(&turn)?;
        turn.transition(TurnStatus::Submitted)
            .map_err(|t| illegal_turn(turn.status, t))?;
        self.sh.store.update_turn(&turn)?;
        // Claim the turn for this live session *before* the submit handshake:
        // the engine can stream and finish the turn while we are still
        // awaiting `port.submit`, and a late claim loses `turn_finished`
        // (the record would never go terminal, and the session would never
        // return to idle).
        {
            let mut g = self.lock();
            if let Some(live) = g.live.as_mut() {
                live.current_turn = Some(turn.id.clone());
                live.current_turn_source = source.map(str::to_string);
                live.turn_started_at = Some(Instant::now());
                live.live_text.clear();
            }
        }
        let wire = format!(
            "{}\n{text}",
            context_line(&self.sh, source.unwrap_or("console"))
        );
        // Background turns (crons, the briefing, `-bg` asks) may run on a
        // cheaper model; everything operator-driven keeps the session one.
        let src = source.unwrap_or("");
        let background = src.starts_with("cron") || src == "briefing" || src.ends_with("-bg");
        let model = background
            .then(|| self.cfg().background_model.clone())
            .flatten();
        match port
            .submit_with_model(wire, Vec::new(), skills, model)
            .await
        {
            Ok(()) => {}
            Err(PortError::EngineClosed) => {
                turn.transition(TurnStatus::Interrupted)
                    .map_err(|t| illegal_turn(turn.status, t))?;
                self.sh.store.update_turn(&turn)?;
                rt_on_engine_closed(&self.sh, port).await;
                return Ok(());
            }
            Err(e) => return Err(e.into()),
        }
        // The engine may have finished the turn already (fast scripted
        // speaker): its record is then terminal and there is nothing left
        // for this handshake to record.
        if self
            .sh
            .store
            .get_turn(&turn.id)
            .ok()
            .flatten()
            .is_some_and(|t| t.status.is_terminal())
        {
            return Ok(());
        }
        turn.transition(TurnStatus::Running)
            .map_err(|t| illegal_turn(turn.status, t))?;
        self.sh.store.update_turn(&turn)?;
        self.sh.emit(AgentEvent::TurnStarted {
            turn: turn.id.to_string(),
            source: source.unwrap_or("").to_string(),
        });
        Ok(())
    }

    /// `/stop`: abort; escalate to SIGKILL after the configured grace
    /// (SEED.md §3.8).
    pub async fn stop_turn(&self) -> Result<(), Error> {
        let port = {
            let mut g = self.lock();
            let status = g.session.status;
            let live_port = g.live.as_ref().map(|l| Arc::clone(&l.port));
            match (status, live_port) {
                (SessionStatus::Running | SessionStatus::AwaitingApproval, Some(p)) => {
                    g.session
                        .transition(SessionStatus::Stopping)
                        .map_err(|t| illegal_session(status, t))?;
                    Some(p)
                }
                _ => None,
            }
        };
        let Some(port) = port else {
            self.sh.emit(AgentEvent::Notice {
                text: "nothing to stop".into(),
            });
            return Ok(());
        };
        self.sh.emit(AgentEvent::Notice {
            text: "stopping…".into(),
        });
        port.abort().await.ok();
        let grace = Duration::from_secs(self.cfg().turn_grace_secs);
        // Escalate on the *turn*, not the process: a successful abort leaves
        // the worker attached (it is reused for the next turn), and the old
        // `is_closed()` test therefore always killed it — SIGKILLing a turn
        // the abort had already drained into.
        let sh = Arc::clone(&self.sh);
        let turn = {
            let g = self.lock();
            g.live.as_ref().and_then(|l| l.current_turn.clone())
        };
        tokio::spawn(async move {
            tokio::time::sleep(grace).await;
            let turn_still_open = turn
                .as_ref()
                .and_then(|t| sh.store.get_turn(t).ok().flatten())
                .is_some_and(|t| !t.status.is_terminal());
            if turn_still_open && !port.is_closed() {
                if let Some(live) = sh
                    .inner
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .live
                    .as_mut()
                {
                    live.deliberate_close = true;
                }
                port.kill().await;
            }
        });
        Ok(())
    }

    /// Skills the engine currently discovers (for `/skills` and doctor).
    /// Provision one skill into the engine home (the `/skills accept`
    /// path): true when the operator's library has it.
    pub fn enable_skill(&self, name: &str) -> Result<bool, Error> {
        let skills = crate::skills::Skills::new(&self.sh.paths.engine_home());
        skills.enable(name).map_err(Error::Skills)
    }

    pub fn skills_enabled(&self) -> Vec<crate::skills::SkillInfo> {
        crate::skills::Skills::new(&self.sh.paths.engine_home()).enabled()
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    fn now() -> jiff::Zoned {
        "2026-07-14T23:14:05+02:00[Europe/Madrid]".parse().unwrap()
    }

    #[test]
    fn ctx_line_names_time_zone_and_channel() {
        let line = ctx_line(&now(), "console");
        assert_eq!(
            line,
            "<ctx now=\"2026-07-14T23:14:05+02:00\" day=\"Tue\" tz=\"Europe/Madrid\" via=\"terminal\"/>"
        );
        assert!(ctx_line(&now(), "telegram:42").contains("via=\"telegram\""));
        assert!(ctx_line(&now(), "matrix:!room").contains("via=\"matrix\""));
        assert!(ctx_line(&now(), "cli-bg").contains("via=\"cli\""));
        assert!(ctx_line(&now(), "web").contains("via=\"web\""));
        assert!(ctx_line(&now(), "api-bg").contains("via=\"api\""));
    }

    #[test]
    fn scheduled_turns_name_their_origin() {
        let cron = ctx_line(&now(), "cron:task_abc");
        assert!(cron.contains("via=\"cron\""), "{cron}");
        assert!(cron.contains("scheduled=\"cron:task_abc\""), "{cron}");
        let brief = ctx_line(&now(), "briefing");
        assert!(brief.contains("via=\"briefing\""), "{brief}");
        assert!(brief.contains("scheduled=\"briefing\""), "{brief}");
        assert!(!ctx_line(&now(), "console").contains("scheduled="));
    }
}
