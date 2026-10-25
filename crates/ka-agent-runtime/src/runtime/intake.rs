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
    sh.surface.on_inbound(&item.source, &item.text);
    publish_turn_source(&sh.store, &item.source);
    let mut turn = Turn::new(sid.clone(), item.text.clone());
    if let Err(e) = sh.store.insert_turn(&turn) {
        sh.surface
            .on_error("internal", &format!("queue drain: {e}"));
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
            live.live_text.clear();
        }
    }
    match port.submit(item.text.clone(), Vec::new(), Vec::new()).await {
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
        self.sh.surface.on_inbound(source, text);
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
                        self.sh.surface.on_notice(
                            "paused: budget reached for this window; use /resume to override or wait for the window to roll",
                        );
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
                    self.sh.surface.on_notice(
                        "⚠ degraded — the worker keeps crashing; /new starts a fresh session",
                    );
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
                    let item = QueueItem::new(self.session_id(), text.to_string(), source);
                    self.sh.store.enqueue(&item)?;
                    let n = self.sh.store.queue_len(&self.session_id())?;
                    self.sh.surface.on_notice(&format!("queued (#{n})"));
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
        match self.submit_text(text).await {
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
    pub async fn handle_skill(&self, name: &str, text: &str) -> Result<(), Error> {
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
        match self.submit_text_with(text, vec![name.to_string()]).await {
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

    async fn submit_text(&self, text: &str) -> Result<(), Error> {
        self.submit_text_with(text, Vec::new()).await
    }

    /// Submit with per-turn skills (`Command::Prompt.skills`: the engine
    /// injects the full SKILL.md bodies for these names into this turn).
    async fn submit_text_with(&self, text: &str, skills: Vec<String>) -> Result<(), Error> {
        if let Err(msg) = self.preflight() {
            self.sh.surface.on_notice(&msg);
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
            self.sh
                .surface
                .on_error("internal", &format!("worker start failed: {e}"));
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
                live.live_text.clear();
            }
        }
        match port.submit(text.to_string(), Vec::new(), skills).await {
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
        self.sh.surface.on_turn_started();
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
            self.sh.surface.on_notice("nothing to stop");
            return Ok(());
        };
        self.sh.surface.on_notice("stopping…");
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
    pub fn skills_enabled(&self) -> Vec<crate::skills::SkillInfo> {
        crate::skills::Skills::new(&self.sh.paths.engine_home()).enabled()
    }
}
