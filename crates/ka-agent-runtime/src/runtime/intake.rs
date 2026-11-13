// Inbound handling per user: principal resolution, preflight, submit, the
// durable queue, retry and stop. Split from runtime.rs (moves only).
use super::worker::rt_on_engine_closed;
use super::*;
use ka_agent_core::Principal;

/// Who started the turn being submitted and which user/role it acts as: the
/// bridge's loop guard and its per-user notes/memory targets.
fn publish_turn_context(store: &Store, source: &str, who: &Principal) {
    // Per-user keys: one worker (and one bridge) serves exactly one user, so
    // this context can never be crossed between concurrent turns. The
    // globals stay written for pre-users readers only.
    for (field, value) in [
        ("source", source.to_string()),
        ("role", who.role.clone()),
        ("username", who.username.clone()),
    ] {
        let _ = store.meta_set(
            &ka_agent_core::layout::turnctx_key(&who.username, field),
            &value,
        );
    }
    let _ = store.meta_set(ka_agent_core::layout::TURN_SOURCE_META_KEY, source);
    let _ = store.meta_set(ka_agent_core::layout::TURN_ROLE_META_KEY, &who.role);
    let _ = store.meta_set(ka_agent_core::layout::TURN_USER_META_KEY, &who.username);
}

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

/// Submit the next queued message of one user, if any. Returns true when a
/// new cycle is running (session stays `running`).
pub(super) async fn drain_queue(sh: &Arc<Shared>, who: &Principal, st: &UserHandle) -> bool {
    let (sid, private) = {
        let g = st.lock();
        // Same guard as the session id: a `/new` between two locks could
        // pair the ended private session's id with the new session's
        // privacy flag.
        (g.session.id.clone(), g.session.private)
    };
    let Some(item) = sh.store.next_queued(&sid).ok().flatten() else {
        return false;
    };
    let port = {
        let g = st.lock();
        g.live.as_ref().map(|l| Arc::clone(&l.port))
    };
    let Some(port) = port else {
        return false;
    };
    // Queued messages cross channels too: the transcript sees them the same
    // as directly-submitted ones (drain bypasses handle_message).
    sh.emit(AgentEvent::Inbound {
        source: item.source.clone(),
        text: item.text.clone(),
    });
    publish_turn_context(&sh.store, &item.source, who);
    // Background turns may run on the background model; the selector the
    // runtime submits on is recorded with the turn (per-model spend).
    let background = item.source.starts_with("cron")
        || item.source == "briefing"
        || item.source.ends_with("-bg");
    // The user's own `/model` choice binds queued messages too — a member's
    // session pick is not a no-op behind a busy turn.
    let session_pick = Runtime { sh: Arc::clone(sh) }.session_model_for(&who.username);
    let configured = session_pick.or_else(|| {
        background
            .then(|| cfg_of(sh).background_model.clone())
            .flatten()
            .or_else(|| cfg_of(sh).model.clone())
    });
    let model =
        model_under_policy(&sh.store, &who.username, configured.as_deref()).unwrap_or_else(|e| {
            sh.emit_from(AgentEvent::Notice { text: e }, Some(&item.source));
            None
        });
    // A private conversation records usage only: the row carries no text.
    let mut turn = Turn::new(
        sid.clone(),
        if private {
            String::new()
        } else {
            item.text.clone()
        },
    );
    turn.model = model.clone();
    if let Err(e) = sh.store.insert_turn(&turn) {
        sh.emit(AgentEvent::Error {
            class: "internal".into(),
            retryable: retryable_class("internal"),
            message: format!("queue drain: {e}"),
        });
        return false;
    }
    let _ = sh.store.meta_set(
        &ka_agent_core::layout::turnctx_key(&who.username, "id"),
        turn.id.as_str(),
    );
    let _ = sh
        .store
        .meta_set(ka_agent_core::layout::TURN_ID_META_KEY, turn.id.as_str());
    sh.store.record_staged_sources(&item.source, turn.id.as_str());
    if turn.transition(TurnStatus::Submitted).is_err() {
        return false;
    }
    sh.store.update_turn(&turn).ok();
    sh.store.mark_turned(&item.id, &turn.id).ok();
    // Claim the turn *before* the submit handshake, exactly like
    // `submit_text_with`: the engine can stream and finish while we are
    // still awaiting `port.submit`.
    {
        let mut g = st.lock();
        if let Some(live) = g.live.as_mut() {
            live.current_turn = Some(turn.id.clone());
            live.current_turn_source = Some(item.source.clone());
            live.turn_started_at = Some(Instant::now());
            live.last_activity_ms = ka_agent_core::now_ms();
            live.watchdog_notified = false;
            live.live_text.clear();
        }
    }
    let wire = format!("{}\n{}", context_line(sh, &item.source), item.text);
    let allowed_tools = Runtime { sh: Arc::clone(sh) }.role_toolset(&who.role);
    match port
        .submit_full(wire, Vec::new(), Vec::new(), model, allowed_tools)
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

/// The model a turn may actually use: the user's policy default when the
/// configured one is not allowed, and a named refusal when nothing in the
/// policy matches. `Ok(None)` = no model configured (allowed for scripted
/// speakers).
fn model_under_policy(
    store: &Store,
    username: &str,
    configured: Option<&str>,
) -> Result<Option<String>, String> {
    let (allow, default) = store
        .model_policy_for(username)
        .map_err(|e| format!("model policy: {e}"))?;
    let Some(configured) = configured else {
        // Nothing configured: the policy default (if the group sets one).
        return Ok(default.filter(|d| ka_agent_store::model_allowed(&allow, d)));
    };
    if ka_agent_store::model_allowed(&allow, configured) {
        return Ok(Some(configured.to_string()));
    }
    // A model outside the policy is refused by name — never silently
    // replaced by the group default.
    Err(format!(
        "model {configured} is not allowed for {username} (allowed: {}); ask an admin to widen your group",
        allow.join(", ")
    ))
}

impl Runtime {
    /// Re-submit the user's newest turn when it ended in `error` or
    /// `interrupted` (F2): the one-command recovery after a provider blip or
    /// a worker crash. Never automatic (ka-port.md §7): only an operator act
    /// re-runs a turn that may have had side effects.
    pub async fn retry_last(&self, who: &Principal) -> Result<Option<String>, Error> {
        if self.session_private_for(who) {
            return Err(Error::Refused(
                "this is a private conversation — its turns are not kept; /new starts a normal one"
                    .into(),
            ));
        }
        let session = self.session_for_user(who)?.id;
        let Some(turn) = self
            .sh
            .store
            .latest_turns_for_session(&session, 1, 0)?
            .into_iter()
            .next()
        else {
            return Ok(None);
        };
        if !matches!(
            turn.status,
            ka_agent_core::TurnStatus::Error | ka_agent_core::TurnStatus::Interrupted
        ) {
            return Ok(None);
        }
        let text = turn.inbound_text.clone();
        // A retry replays stored text that may already contain restricted
        // `@kb:`/file content: carry the original turn's provenance rows so
        // the share-time check sees the same sources (M3).
        if let Ok(rows) = self.sh.store.turn_sources(turn.id.as_str())
            && !rows.is_empty()
        {
            let staged: Vec<ka_agent_store::StagedSource> = rows
                .into_iter()
                .map(|r| ka_agent_store::StagedSource {
                    kind: r.kind,
                    reference: r.reference,
                    groups: r.group_names,
                })
                .collect();
            self.sh.store.stage_turn_sources("retry", &staged);
        }
        self.handle_message_as(&text, "retry", who).await?;
        Ok(Some(text))
    }

    /// Inbound message: resolve the sender's principal (an unlinked identity
    /// or a disabled account is refused and audited), then submit or queue
    /// for that user.
    pub async fn handle_message(&self, text: &str, source: &str) -> Result<(), Error> {
        match self.principal_for_source(source) {
            Some(who) => self.handle_message_as(text, source, &who).await,
            None => {
                self.refuse_unknown_sender(source);
                Ok(())
            }
        }
    }

    /// Refuse an inbound message from a source with no principal: no reply,
    /// one `<channel>.unauthorized` audit row (surfaced on `/contacts`).
    fn refuse_unknown_sender(&self, source: &str) {
        let channel = source
            .strip_suffix("-bg")
            .unwrap_or(source)
            .split(':')
            .next()
            .unwrap_or(source)
            .to_string();
        let action = format!("{channel}.unauthorized");
        let mut rec = AuditRecord::new(source, &action, "identity", Some("unlinked or disabled".into()));
        let _ = self.sh.store.append_audit(&mut rec);
    }

    /// Inbound message for an explicit principal (cron jobs carry their
    /// creator's user; every surface resolves it from the source).
    pub async fn handle_message_as(
        &self,
        text: &str,
        source: &str,
        who: &Principal,
    ) -> Result<(), Error> {
        self.handle_message_inner(text, source, who, None).await
    }

    /// A cron fire: its creator's principal and its (already clamped) tool
    /// narrowing. `None` = the job requests no narrowing; `Some(list)` is an
    /// allowlist even when the clamp emptied it (never a widening).
    pub async fn handle_cron_message(
        &self,
        text: &str,
        source: &str,
        who: &Principal,
        tools: Option<Vec<String>>,
    ) -> Result<(), Error> {
        self.handle_message_inner(text, source, who, tools).await
    }

    async fn handle_message_inner(
        &self,
        text: &str,
        source: &str,
        who: &Principal,
        tool_override: Option<Vec<String>>,
    ) -> Result<(), Error> {
        let st = self.ensure_user(who)?;
        // The user's own state decides: idle submits, a live turn steers or
        // queues (see `handle_message_active`).
        self.handle_message_active(text, source, who, &st, tool_override)
            .await
    }

    /// Submit now when idle, otherwise enqueue durably (gap G4 — ka drops
    /// mid-cycle prompts, HTTP 200 notwithstanding).
    async fn handle_message_active(
        &self,
        text: &str,
        source: &str,
        who: &Principal,
        st: &UserHandle,
        tool_override: Option<Vec<String>>,
    ) -> Result<(), Error> {
        self.sh.emit(AgentEvent::Inbound {
            source: source.to_string(),
            text: text.to_string(),
        });
        let mut submit_head = false;
        let submit_now = {
            let mut g = st.lock();
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
                SessionStatus::Running | SessionStatus::AwaitingApproval | SessionStatus::Stopping => {
                    false
                }
                SessionStatus::Paused => {
                    // A budget pause lifts itself when the day window rolls
                    // (the verdict is recomputed from the recorded window);
                    // an auth pause needs /resume.
                    if g.budget.verdict() == ka_agent_core::BudgetVerdict::Exceeded {
                        self.sh.emit_from(
                            AgentEvent::Notice {
                                text: "paused: budget reached for this window; use /resume to override or wait for the window to roll"
                                    .into(),
                            },
                            Some(source),
                        );
                        false
                    } else {
                        // Paused→Running is not a legal edge; go through idle.
                        g.session
                            .transition(SessionStatus::Idle)
                            .map_err(|t| illegal_session(SessionStatus::Paused, t))?;
                        g.session
                            .transition(SessionStatus::Running)
                            .map_err(|t| illegal_session(SessionStatus::Idle, t))?;
                        let sid = g.session.id.clone();
                        drop(g);
                        self.persist_session_status(&who.username, &sid, SessionStatus::Idle);
                        true
                    }
                }
                SessionStatus::Degraded => {
                    self.sh.emit_from(
                        AgentEvent::Notice {
                            text: "⚠ degraded — the worker keeps crashing; /new starts a fresh session"
                                .into(),
                        },
                        Some(source),
                    );
                    false
                }
            }
        };
        if submit_now {
            // Persist immediately: the store is the durable source of truth
            // for recovery.
            let (sid, status) = {
                let g = st.lock();
                (g.session.id.clone(), g.session.status)
            };
            self.persist_session_status(&who.username, &sid, status);
        }
        if !submit_now {
            match self.session_status_for(who) {
                SessionStatus::Running | SessionStatus::AwaitingApproval | SessionStatus::Stopping => {
                    // `busy_messages = "steer"`: an operator message lands in
                    // the running reply at the next tool-batch boundary.
                    // Scheduled turns stay uninterrupted.
                    let live = self.live_port_for(who);
                    let running = self.current_turn_source_for(who);
                    let steerable = self.cfg().busy_messages == "steer"
                        && live.is_some()
                        && !running.as_deref().is_some_and(|s| {
                            s == "cron" || s.starts_with("cron:") || s == "briefing"
                        });
                    if steerable
                        && let Some(port) = live
                        && port.steer(text.to_string()).await.is_ok()
                    {
                        let (turn_id, private) = {
                            let g = st.lock();
                            (
                                g.live.as_ref().and_then(|l| l.current_turn.clone()),
                                g.session.private,
                            )
                        };
                        if let (Some(id), false) = (&turn_id, private) {
                            let _ = self
                                .sh
                                .store
                                .append_turn_inbound(id, &format!("\n\n[added while working] {text}"));
                        }
                        if let Some(live) = st.lock().live.as_mut() {
                            live.live_text
                                .push_str(&format!("\n\n[added while working] {text}"));
                        }
                        self.sh.emit_from(
                            AgentEvent::Note {
                                text: "steered into the running reply".into(),
                            },
                            Some(source),
                        );
                        return Ok(());
                    }
                    let item = QueueItem::new(
                        st.lock().session.id.clone(),
                        text.to_string(),
                        source,
                    );
                    self.sh.store.enqueue(&item)?;
                    let n = self.sh.store.queue_len(&st.lock().session.id)?;
                    self.sh.emit(AgentEvent::Queue { depth: n as u32 });
                    self.sh.emit_from(
                        AgentEvent::Notice {
                            text: format!("queued (#{n})"),
                        },
                        Some(source),
                    );
                }
                _ => {}
            }
            return Ok(());
        }
        if submit_head {
            if let Err(e) = self.ensure_worker(who, source).await {
                let mut g = st.lock();
                if g.session.status == SessionStatus::Running {
                    g.session.transition(SessionStatus::Idle).ok();
                }
                let sid = g.session.id.clone();
                drop(g);
                self.persist_session_status(&who.username, &sid, SessionStatus::Idle);
                // Over the worker cap the message stays queued for a slot.
                if matches!(e, Error::Refused(_)) {
                    self.sh.emit_from(
                        AgentEvent::Notice {
                            text: e.to_string(),
                        },
                        Some(source),
                    );
                    return Ok(());
                }
                return Err(e);
            }
            drain_queue(&self.sh, who, st).await;
            return Ok(());
        }
        match self
            .submit_text_with(text, Vec::new(), Some(source), who, tool_override)
            .await
        {
            Ok(()) => Ok(()),
            Err(e) => {
                // Never strand the session in `running`: the queue would
                // swallow every later message (single-flight).
                let mut g = st.lock();
                if g.session.status == SessionStatus::Running {
                    g.session.transition(SessionStatus::Idle).ok();
                }
                let sid = g.session.id.clone();
                drop(g);
                self.persist_session_status(&who.username, &sid, SessionStatus::Idle);
                Err(e)
            }
        }
    }

    /// `/skill <name> <text>`: submit one turn with that skill's full body
    /// injected (SEED.md §2.6, §9.1). Fails closed: the skill must be
    /// provisioned in the user's engine home, and the session must be idle.
    pub async fn handle_skill(
        &self,
        name: &str,
        text: &str,
        source: &str,
        who: &Principal,
    ) -> Result<(), Error> {
        let st = self.ensure_user(who)?;
        let skills = crate::skills::Skills::new(
            &self.sh.paths.user_engine_home(&who.username),
        );
        if !skills.is_enabled(name)? {
            return Err(Error::Refused(format!(
                "skill {name:?} is not enabled — `ka-agent skills enable {name}`"
            )));
        }
        if text.trim().is_empty() {
            return Err(Error::Refused("usage: /skill <name> <text>".into()));
        }
        {
            let mut g = st.lock();
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
        {
            let (sid, status) = {
                let g = st.lock();
                (g.session.id.clone(), g.session.status)
            };
            self.persist_session_status(&who.username, &sid, status);
        }
        match self
            .submit_text_with(text, vec![name.to_string()], Some(source), who, None)
            .await
        {
            Ok(()) => Ok(()),
            Err(e) => {
                let mut g = st.lock();
                if g.session.status == SessionStatus::Running {
                    g.session.transition(SessionStatus::Idle).ok();
                }
                let sid = g.session.id.clone();
                drop(g);
                self.persist_session_status(&who.username, &sid, SessionStatus::Idle);
                Err(e)
            }
        }
    }

    /// Refuse a message that cannot possibly work, before any state moves:
    /// no model configured, the model's provider key missing, or a model
    /// outside the user's policy.
    pub(super) fn preflight(&self, who: &Principal) -> Result<(), String> {
        // Scripted speakers (e2e) legitimately run without model or keys.
        if self.sh.port.scenario_override.is_some() {
            return Ok(());
        }
        let cfg = self.cfg();
        let model = model_under_policy(&self.sh.store, &who.username, cfg.model.as_deref())?;
        if model.is_none() && cfg.ka.engine_layer.is_none() {
            return Err(
                "no model configured — run `ka-agent setup` (or /model provider/model)".into(),
            );
        }
        if let Some(model) = &model {
            // Validate once per model string; re-read the .env file only
            // when its mtime moved (preflight runs on every submit).
            let sel = {
                let mut cache = self
                    .sh
                    .selector_cache
                    .lock()
                    .unwrap_or_else(|e| e.into_inner());
                if let Some((cached_model, sel)) = cache.as_ref()
                    && cached_model == model
                {
                    sel.clone()
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
                if let Some((cached_mtime, env)) = cache.as_ref()
                    && *cached_mtime == mtime
                {
                    env.clone()
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
        skills_extra: Vec<String>,
        source: Option<&str>,
        who: &Principal,
        tool_override: Option<Vec<String>>,
    ) -> Result<(), Error> {
        let st = self.ensure_user(who)?;
        // The bridge reads the turn's source, user and role from the store
        // while the turn runs.
        publish_turn_context(&self.sh.store, source.unwrap_or("console"), who);
        if let Err(msg) = self.preflight(who) {
            self.sh.emit_from(
                AgentEvent::Notice { text: msg },
                source,
            );
            let mut g = st.lock();
            if g.session.status == SessionStatus::Running {
                g.session.transition(SessionStatus::Idle).ok();
            }
            let sid = g.session.id.clone();
            drop(g);
            self.persist_session_status(&who.username, &sid, SessionStatus::Idle);
            self.sh.emit(AgentEvent::Session {
                status: "Idle".into(),
            });
            return Ok(());
        }
        if let Err(e) = self.ensure_worker(who, source.unwrap_or("console")).await {
            if matches!(e, Error::Refused(_)) {
                // Over the worker cap: queue durably and try again when a
                // slot frees (never a silent drop).
                let sid = st.lock().session.id.clone();
                let item = QueueItem::new(
                    sid.clone(),
                    text.to_string(),
                    source.unwrap_or("console"),
                );
                self.sh.store.enqueue(&item)?;
                let mut g = st.lock();
                if g.session.status == SessionStatus::Running {
                    g.session.transition(SessionStatus::Idle).ok();
                }
                drop(g);
                self.persist_session_status(&who.username, &sid, SessionStatus::Idle);
                self.sh.emit_from(
                    AgentEvent::Notice {
                        text: e.to_string(),
                    },
                    source,
                );
                return Ok(());
            }
            // A dead worker at submit time behaves like an interruption:
            // surface it, never auto-retry (SEED.md §3.8).
            self.sh.emit_from(
                AgentEvent::Error {
                    class: "internal".into(),
                    retryable: retryable_class("internal"),
                    message: format!("worker start failed: {e}"),
                },
                source,
            );
            let mut g = st.lock();
            let from = g.session.status;
            g.session
                .transition(SessionStatus::Idle)
                .map_err(|t| illegal_session(from, t))?;
            return Ok(());
        }
        let (sid, private, port) = {
            let g = st.lock();
            let Some(live) = g.live.as_ref() else {
                return Ok(());
            };
            (
                g.session.id.clone(),
                g.session.private,
                Arc::clone(&live.port),
            )
        };
        // Record the turn BEFORE submitting (gap G8: ka persists per cycle,
        // so Ka Agent's record is the durable one). Background turns (crons,
        // the briefing, `-bg` asks) may run on a cheaper model.
        let src = source.unwrap_or("");
        let background = src.starts_with("cron") || src == "briefing" || src.ends_with("-bg");
        // Same precedence as the drain path: the user's session pick wins
        // over background/configured (policy was enforced at `/model` time).
        let model = model_under_policy(
            &self.sh.store,
            &who.username,
            self.session_model_for(&who.username)
                .or_else(|| {
                    background
                        .then(|| self.cfg().background_model.clone())
                        .flatten()
                        .or_else(|| self.cfg().model.clone())
                })
                .as_deref(),
        )
        .unwrap_or_else(|e| {
            self.sh.emit_from(
                AgentEvent::Notice { text: e },
                source,
            );
            None
        });
        // A private conversation records usage only: the row carries no text.
        let mut turn = Turn::new(
            sid,
            if private {
                String::new()
            } else {
                text.to_string()
            },
        );
        turn.model = model.clone();
        self.sh.store.insert_turn(&turn)?;
        let _ = self.sh.store.meta_set(
            &ka_agent_core::layout::turnctx_key(&who.username, "id"),
            turn.id.as_str(),
        );
        let _ = self
            .sh
            .store
            .meta_set(ka_agent_core::layout::TURN_ID_META_KEY, turn.id.as_str());
        self.sh
            .store
            .record_staged_sources(source.unwrap_or("console"), turn.id.as_str());
        turn.transition(TurnStatus::Submitted)
            .map_err(|t| illegal_turn(turn.status, t))?;
        self.sh.store.update_turn(&turn)?;
        // Claim the turn for this live session *before* the submit handshake.
        {
            let mut g = st.lock();
            if let Some(live) = g.live.as_mut() {
                live.current_turn = Some(turn.id.clone());
                live.current_turn_source = source.map(str::to_string);
                live.turn_started_at = Some(Instant::now());
                live.last_activity_ms = ka_agent_core::now_ms();
                live.watchdog_notified = false;
            }
        }
        // A shared agent contributes its persona, skills and model to this
        // turn; the user's own policy still bounds the model.
        let agent = self.active_agent(who);
        let (agent_preamble, mut skills, agent_model) = match &agent {
            Some(a) => (
                format!(
                    "<agent name=\"{}\" owner=\"{}\">\n{}\n</agent>\n",
                    a.name, a.owner_username, a.persona
                ),
                a.skills.clone(),
                a.model.clone(),
            ),
            None => (String::new(), Vec::new(), None),
        };
        skills.extend(skills_extra);
        let model = match agent_model {
            Some(m) => {
                let (allow, _) = self
                    .sh
                    .store
                    .model_policy_for(&who.username)
                    .unwrap_or_default();
                if !ka_agent_store::model_allowed(&allow, &m) {
                    self.sh.emit_from(
                        AgentEvent::Notice {
                            text: format!(
                                "agent model {m} is not allowed for {} — pick another agent or widen the group",
                                who.username
                            ),
                        },
                        source,
                    );
                    None
                } else {
                    Some(m)
                }
            }
            None => model,
        };
        let wire = format!(
            "{}{}\n{text}",
            context_line(&self.sh, source.unwrap_or("console")),
            agent_preamble
        );
        if let Some(a) = &agent {
            let _ = self.sh.store.meta_set(
                &ka_agent_core::layout::turnctx_key(&who.username, "agent"),
                &a.name,
            );
        }
        // The per-turn allowlist: the role's toolset, narrowed by a cron's
        // own list (never widened).
        let role_allow = self.role_toolset(&who.role);
        let allowed_tools = match tool_override {
            Some(tools) => Some(match role_allow {
                Some(allow) => tools.into_iter().filter(|t| allow.contains(t)).collect(),
                None => tools,
            }),
            None => role_allow,
        };
        match port
            .submit_full(wire, Vec::new(), skills, model, allowed_tools)
            .await
        {
            Ok(()) => {}
            Err(PortError::EngineClosed) => {
                turn.transition(TurnStatus::Interrupted)
                    .map_err(|t| illegal_turn(turn.status, t))?;
                self.sh.store.update_turn(&turn)?;
                rt_on_engine_closed(&self.sh, &st, port).await;
                return Ok(());
            }
            Err(e) => return Err(e.into()),
        }
        // The engine may have finished the turn already (fast scripted
        // speaker): its record is then terminal.
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
        Ok(())
    }

    /// `/stop`: abort; escalate to SIGKILL after the configured grace
    /// (SEED.md §3.8).
    pub async fn stop_turn(&self, who: &Principal, source: &str) -> Result<(), Error> {
        let st = self.ensure_user(who)?;
        let port = {
            let mut g = st.lock();
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
            self.sh.emit_from(
                AgentEvent::Notice {
                    text: "nothing to stop".into(),
                },
                Some(source),
            );
            return Ok(());
        };
        self.sh.emit_from(
            AgentEvent::Notice {
                text: "stopping…".into(),
            },
            Some(source),
        );
        port.abort().await.ok();
        let grace = Duration::from_secs(self.cfg().turn_grace_secs);
        // Escalate on the *turn*, not the process: a successful abort leaves
        // the worker attached (it is reused for the next turn).
        let sh = Arc::clone(&self.sh);
        let turn = { st.lock().live.as_ref().and_then(|l| l.current_turn.clone()) };
        tokio::spawn(async move {
            tokio::time::sleep(grace).await;
            let turn_still_open = turn
                .as_ref()
                .and_then(|t| sh.store.get_turn(t).ok().flatten())
                .is_some_and(|t| !t.status.is_terminal());
            if turn_still_open && !port.is_closed() {
                if let Some(live) = st.lock().live.as_mut() {
                    live.deliberate_close = true;
                }
                port.kill().await;
            }
        });
        Ok(())
    }

    /// Provision one skill into the user's engine home.
    pub fn enable_skill(&self, name: &str, who: &Principal) -> Result<bool, Error> {
        let skills = crate::skills::Skills::new(&self.sh.paths.user_engine_home(&who.username));
        skills.enable(name).map_err(Error::Skills)
    }

    /// Skills the user's engine currently discovers.
    pub fn skills_enabled(&self, who: &Principal) -> Vec<crate::skills::SkillInfo> {
        crate::skills::Skills::new(&self.sh.paths.user_engine_home(&who.username)).enabled()
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

    #[test]
    fn model_policy_resolution() {
        let store = Store::open_in_memory().unwrap();
        store
            .create_user(&ka_agent_store::UserRow {
                username: "ana".into(),
                display_name: "Ana".into(),
                pw_hash: None,
                role: "member".into(),
                quota_daily_usd: None,
                disabled: false,
                created_ms: 1,
                created_by: Some("test".into()),
            })
            .unwrap();
        // No groups: every configured model is allowed.
        assert_eq!(
            model_under_policy(&store, "ana", Some("openai/gpt-5.2")).unwrap(),
            Some("openai/gpt-5.2".into())
        );
        // A group with an allowlist refuses a foreign model by name.
        store
            .upsert_group("dev", &["anthropic/*".into()], Some("anthropic/claude"))
            .unwrap();
        store.add_user_group("ana", "dev").unwrap();
        assert!(
            model_under_policy(&store, "ana", Some("openai/gpt-5.2"))
                .unwrap_err()
                .contains("not allowed for ana"),
            "a disallowed configured model is refused by name"
        );
        // No configured model: the policy default applies.
        assert_eq!(
            model_under_policy(&store, "ana", None).unwrap(),
            Some("anthropic/claude".into())
        );
        assert_eq!(
            model_under_policy(&store, "ana", Some("anthropic/claude-3")).unwrap(),
            Some("anthropic/claude-3".into())
        );
    }
}
