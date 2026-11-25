// Inbound handling per user: principal resolution, preflight, submit, the
// durable queue, retry and stop. Split from runtime.rs (moves only).
use super::events::is_scheduled;
use super::scheduler::record_trigger_run;
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

/// A per-turn override (`/skill` allowed_tools, a cron's tool list) can
/// only narrow the role's toolset, never widen it.
fn narrow_tools(
    role_allow: Option<Vec<String>>,
    tool_override: Option<Vec<String>>,
) -> Option<Vec<String>> {
    match tool_override {
        Some(tools) => Some(match role_allow {
            Some(allow) => tools.into_iter().filter(|t| allow.contains(t)).collect(),
            None => tools,
        }),
        None => role_allow,
    }
}

/// What one submit attaches beyond its explicit list: the trigger/paths
/// skills and the per-kind names `/skill-doctor` counts.
#[derive(Default)]
struct SkillAttach {
    /// Names to put on the wire (explicit + trigger + paths, deduped).
    attach: Vec<String>,
    explicit: Vec<String>,
    trigger: Vec<String>,
    paths: Vec<String>,
}

/// Resolve the skills a submission fires. Trigger matching and paths
/// attachment are conversational only: cron/briefing turns are exempt (a
/// pending paths skill waits for the next human turn).
fn plan_skill_attach(
    sh: &Shared,
    who: &Principal,
    st: &UserHandle,
    sid: &str,
    text: &str,
    explicit_names: Option<&[String]>,
    conversational: bool,
) -> SkillAttach {
    let mut out = SkillAttach::default();
    if !conversational {
        return out;
    }
    let resolved = crate::skills::Skills::new(&sh.paths.user_engine_home(&who.username)).resolved();
    let hits = crate::skills::matched_triggers(text, &resolved);
    let mut names: Vec<String> = Vec::new();
    if let Some(explicit) = explicit_names {
        names.extend(explicit.iter().cloned());
    }
    for hit in &hits {
        if !names.contains(hit) {
            names.push(hit.clone());
        }
    }
    names.truncate(3);
    out.attach = names;
    out.trigger = hits
        .into_iter()
        .filter(|hit| out.attach.contains(hit))
        .collect();
    if let Some(explicit) = explicit_names {
        out.explicit = explicit
            .iter()
            .filter(|name| out.attach.contains(name))
            .cloned()
            .collect();
    }
    // A paths touch parks a name until the next submit; the marker is
    // written only when the skill actually attaches, so a restart between
    // touch and attach forgets the touch without burning the marker.
    let pending: Vec<String> = st.lock().paths_pending.iter().cloned().collect();
    let mut keep: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    for name in pending {
        let still_offered = resolved.iter().any(|skill| skill.info.name == name);
        if !still_offered || crate::skills::path_marked(&sh.store, sid, &name) {
            continue;
        }
        keep.insert(name.clone());
        if !out.attach.contains(&name) {
            out.attach.push(name.clone());
        }
        out.paths.push(name);
    }
    if keep.len() != st.lock().paths_pending.len() {
        st.lock().paths_pending = keep;
    }
    out
}

/// Persist the attach counts (`/skill-doctor`) and the paths markers.
/// Called only after the submit handshake succeeded: nothing counted that
/// the engine never saw.
fn record_skill_attach(
    sh: &Shared,
    who: &Principal,
    st: &UserHandle,
    sid: &str,
    attach: &SkillAttach,
) {
    let mut rows: std::collections::BTreeMap<String, (bool, bool, bool)> =
        std::collections::BTreeMap::new();
    for name in &attach.explicit {
        rows.entry(name.clone()).or_default().0 = true;
    }
    for name in &attach.trigger {
        rows.entry(name.clone()).or_default().1 = true;
    }
    for name in &attach.paths {
        rows.entry(name.clone()).or_default().2 = true;
    }
    for (name, (explicit, trigger, paths)) in rows {
        let _ = sh
            .store
            .bump_skill_usage(&who.username, &name, explicit, trigger, paths);
        if paths {
            let _ = sh
                .store
                .meta_set(&crate::skills::path_marker_key(sid, &name), "1");
        }
    }
    if !attach.paths.is_empty() {
        let mut g = st.lock();
        for name in &attach.paths {
            g.paths_pending.remove(name);
        }
    }
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
    // a cron:<id> or trigger:<id> source names the task in `scheduled`.
    let (via, scheduled) = if let Some(rest) = source.strip_prefix("cron:") {
        ("cron", Some(format!("cron:{rest}")))
    } else if source == "cron" {
        ("cron", Some("cron".to_string()))
    } else if source == "briefing" {
        ("briefing", Some("briefing".to_string()))
    } else if let Some(rest) = source.strip_prefix("trigger:") {
        ("trigger", Some(format!("trigger:{rest}")))
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

/// The trigger fire's run row: what the turn did, with the turn's own
/// usage (the `runs`-per-fire rule the cron arm follows).
fn record_trigger_run_finished(
    store: &Store,
    task_id: &ka_agent_core::TaskId,
    stop: &str,
    cost_usd: f64,
    tokens: u64,
    started_ms: i64,
) {
    let _ = store.insert_run(&ka_agent_store::RunRow {
        id: ka_agent_core::RunId::generate(),
        kind: "trigger".into(),
        ref_id: Some(task_id.as_str().to_string()),
        status: if stop == "done" || stop == "length" {
            "fired"
        } else {
            "failed"
        }
        .into(),
        stop: Some(stop.to_string()),
        error_class: None,
        error_message: None,
        cost_usd: Some(cost_usd),
        tokens: Some(tokens),
        started_ms,
        ended_ms: Some(ka_agent_core::now_ms()),
    });
}

/// Index the trigger turn into the episode FTS (user + assistant rows),
/// so the lineage session is searchable and `@turn:`-addressable like a
/// chat transcript.
fn index_trigger_episode(
    sh: &Arc<Shared>,
    sid: &ka_agent_core::SessionId,
    inbound: &str,
    answer: &str,
) {
    let turns = sh
        .store
        .latest_turns_for_session(sid, 1, 0)
        .unwrap_or_default();
    let Some(turn) = turns.last() else {
        return;
    };
    let tid = turn.id.clone();
    if !inbound.trim().is_empty() {
        sh.store.index_episode(sid, &tid, "user", inbound).ok();
    }
    if !answer.trim().is_empty() {
        sh.store.index_episode(sid, &tid, "assistant", answer).ok();
    }
}

/// The bare interrupt words that abort a running turn (A3, Goose's
/// heuristic): `stop halt cease quit end abort cancel wait hold pause`.
/// The medium words (`no`, `wrong`, `actually`, `nevermind`, …) are
/// deliberately NOT acted on — a mid-cycle correction rides the steer
/// path (`busy_messages = "steer"`).
const INTERRUPT_WORDS: &[&str] = &[
    "stop", "halt", "cease", "quit", "end", "abort", "cancel", "wait", "hold", "pause",
];

/// The pure matcher (unit-tested): a trimmed message of at most 20 chars
/// that is exactly one interrupt word — case-insensitive — plus optional
/// trailing punctuation. Anything longer or wordier is an ordinary
/// message: by construction the benign shorts ("ok", "thanks", "hm")
/// never match.
fn is_bare_interrupt(text: &str) -> bool {
    let trimmed = text.trim();
    if trimmed.chars().count() > 20 {
        return false;
    }
    let bare = trimmed.trim_end_matches(['.', '!', '?', '…', ';', ':']);
    if bare.is_empty() {
        return false;
    }
    let lower = bare.to_lowercase();
    INTERRUPT_WORDS.contains(&lower.as_str())
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
    let _ = sh.store.set_turn_origin(&turn.id, &item.source);
    let _ = sh.store.meta_set(
        &ka_agent_core::layout::turnctx_key(&who.username, "id"),
        turn.id.as_str(),
    );
    let _ = sh
        .store
        .meta_set(ka_agent_core::layout::TURN_ID_META_KEY, turn.id.as_str());
    sh.store
        .record_staged_sources(&item.source, turn.id.as_str());
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
            // A nudge item (the empty-answer re-ask) claims its turn as
            // such: if it also comes back empty, the cycle gives up
            // instead of nudging again.
            live.turn_from_nudge = item.text.starts_with(NUDGE_PREFIX);
            live.live_text.clear();
            live.thought_tail.clear();
        }
    }
    // Queued items stay plain text; triggers and parked paths fire at
    // drain time, just like a direct submit (scheduled fires are exempt).
    let attach = plan_skill_attach(
        sh,
        who,
        st,
        sid.as_str(),
        &item.text,
        None,
        !is_scheduled(Some(item.source.as_str())),
    );
    let recall = Runtime { sh: Arc::clone(sh) }
        .recall_block_for(who, &item.source, &item.text)
        .unwrap_or_default();
    let wire = format!(
        "{}\n{}{}",
        context_line(sh, &item.source),
        recall,
        item.text
    );
    let allowed_tools = Runtime { sh: Arc::clone(sh) }.role_toolset(&who.role);
    // B15b: a scheduled turn runs against the automation compile (machine
    // facts only); every exit of this bracket restores the full one — the
    // Ok path at TurnFinished/Idle, the Err path right here (the turn is
    // marked interrupted and no port event will ever arrive).
    let scheduled = is_scheduled(Some(item.source.as_str()));
    if scheduled
        && let Err(e) =
            (Runtime { sh: Arc::clone(sh) }).install_automation_memory_for(&who.username)
    {
        sh.emit(AgentEvent::Error {
            class: "internal".into(),
            retryable: retryable_class("internal"),
            message: format!("automation memory compile: {e}"),
        });
    }
    match port
        .submit_full(
            wire,
            Vec::new(),
            attach.attach.clone(),
            model,
            allowed_tools,
        )
        .await
    {
        Ok(()) => {
            record_skill_attach(sh, who, st, sid.as_str(), &attach);
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
            if scheduled
                && let Err(e) =
                    (Runtime { sh: Arc::clone(sh) }).refresh_compiled_memory_for(&who.username)
            {
                sh.emit(AgentEvent::Error {
                    class: "internal".into(),
                    retryable: retryable_class("internal"),
                    message: format!("memory restore: {e}"),
                });
            }
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
        let mut rec = AuditRecord::new(
            source,
            &action,
            "identity",
            Some("unlinked or disabled".into()),
        );
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

    /// A trigger fire (ADR-0005): one ephemeral worker bound to the
    /// trigger's pinned lineage session — never the creator's live chat, so
    /// automation noise stays out of human conversation. The caller (the
    /// scheduler's `trigger` arm) has claimed the task row and checked the
    /// in-flight cap; this owns the turn row, the run row, the error
    /// streak and the requeue, exactly one fire at a time.
    pub(super) async fn handle_trigger_message(
        &self,
        task: ka_agent_core::Task,
        spec: ka_agent_core::TriggerSpec,
        who: Principal,
        lineage_sid: ka_agent_core::SessionId,
        delivered: Option<ka_agent_core::QueueItem>,
        gate_output: String,
    ) {
        let source = crate::runtime::scheduler::trigger_source(&task);
        let text = delivered
            .as_ref()
            .map(|item| item.text.clone())
            .unwrap_or_else(|| spec.text.clone());
        // The one line of chat-shaped context the model sees (its clock,
        // its channel); scheduled turns name their origin.
        let recall = self
            .recall_block_for(&who, &source, &text)
            .unwrap_or_default();
        let mut wire = format!("{}\n{}{}", context_line(&self.sh, &source), recall, text);
        if !gate_output.trim().is_empty() {
            wire = format!(
                "{wire}\n<gate-output>\n{}\n</gate-output>",
                gate_output.trim_end()
            );
        }
        let started_ms = ka_agent_core::now_ms();
        // Model under the creator's policy (the background chain, like the
        // drain path): a named policy refusal is a failed fire, never a
        // silent fallback to a wider model.
        let cfg = self.cfg();
        let configured = cfg.background_model.clone().or_else(|| cfg.model.clone());
        let model = match model_under_policy(&self.sh.store, &who.username, configured.as_deref()) {
            Ok(model) => model,
            Err(why) => {
                self.sh.emit(AgentEvent::Error {
                    class: "trigger".into(),
                    retryable: retryable_class("trigger"),
                    message: format!("{}: {source}: {why}", task.id.as_str()),
                });
                record_trigger_run(&self.sh.store, &task.id, "failed", Some(&why));
                self.settle_trigger(&task, &spec, false).await;
                return;
            }
        };
        // Tool narrowing: the cron rule — an empty `spec.tools` means the
        // trigger never asked to narrow; otherwise the clamp is the
        // allowlist even when it removed every tool. The clamp uses the
        // fire-time role (the caller re-resolved it).
        let allowed_tools =
            (!spec.tools.is_empty()).then(|| self.clamp_cron_tools(&who.role, &spec.tools));
        // Pool discipline: a fully busy pool skips this occurrence (the
        // queued event survives; the next arm delivers it).
        {
            let _spawn = self.sh.spawn_lock.lock().await;
            if self.live_worker_count() >= self.max_workers() {
                let why = format!(
                    "all {} engine workers busy — the fire waits for the next arm",
                    self.max_workers()
                );
                record_trigger_run(&self.sh.store, &task.id, "skipped", Some(&why));
                self.requeue_trigger(&task, &spec);
                return;
            }
        }
        match self
            .run_trigger_turn(
                &task,
                &who,
                &lineage_sid,
                delivered.as_ref(),
                &text,
                &wire,
                model,
                allowed_tools,
                started_ms,
            )
            .await
        {
            Ok((stop, cost_usd, tokens, answer)) => {
                record_trigger_run_finished(
                    &self.sh.store,
                    &task.id,
                    &stop,
                    cost_usd,
                    tokens,
                    started_ms,
                );
                // FTS transcript: the lineage session stays addressable
                // (`@turn:`/`@session:`), exactly like a chat turn.
                index_trigger_episode(&self.sh, &lineage_sid, &text, &answer);
                self.settle_trigger(&task, &spec, true).await;
            }
            Err(why) => {
                record_trigger_run(&self.sh.store, &task.id, "failed", Some(&why));
                self.settle_trigger(&task, &spec, false).await;
            }
        }
    }

    /// The trigger's post-fire bookkeeping: reset the error streak on
    /// success (and requeue), or count the failure — at
    /// [`TriggerSpec::ERROR_STRIKES`] consecutive failures the trigger
    /// pauses as `interrupted` with exactly one Error notice (ADR-0005
    /// §8); run history is never deleted.
    async fn settle_trigger(
        &self,
        task: &ka_agent_core::Task,
        spec: &ka_agent_core::TriggerSpec,
        ok: bool,
    ) {
        let mut spec = spec.clone();
        if ok {
            spec.error_streak = 0;
            let _ = self
                .sh
                .store
                .update_task_payload(&task.id, &spec.to_payload());
            match spec.kind {
                ka_agent_core::TriggerKind::Schedule => {
                    let recurring = spec.recurrence_ms.is_some_and(|rec| rec > 0);
                    if recurring {
                        let now_zoned = jiff::Timestamp::now().to_zoned(self.cfg().time_zone());
                        match crate::runtime::scheduler::trigger_next_after(
                            &spec,
                            spec.recurrence_ms.unwrap_or(0),
                            &now_zoned,
                        ) {
                            Some(next) => {
                                let _ = self.sh.store.rearm_task(&task.id, next);
                            }
                            None => {
                                let _ = self.sh.store.settle_task(&task.id, "failed");
                            }
                        }
                    } else {
                        let _ = self.sh.store.settle_task(&task.id, "succeeded");
                    }
                }
                ka_agent_core::TriggerKind::Webhook => {
                    let _ = self.sh.store.park_trigger(&task.id);
                }
            }
            return;
        }
        spec.error_streak = spec.error_streak.saturating_add(1);
        let strikes = spec.error_streak;
        let _ = self
            .sh
            .store
            .update_task_payload(&task.id, &spec.to_payload());
        if strikes >= ka_agent_core::TriggerSpec::ERROR_STRIKES
            && self
                .sh
                .store
                .settle_task(&task.id, "interrupted")
                .unwrap_or(false)
        {
            self.sh.emit(AgentEvent::Error {
                class: "trigger".into(),
                retryable: false,
                message: format!(
                    "trigger '{}' paused after {strikes} consecutive failed fires — \
                     `ka-agent cron resume {}` re-arms it",
                    spec.text,
                    ka_agent_core::ids::short_id(task.id.as_str())
                ),
            });
            return;
        }
        // Still below the strike line: keep the trigger working. A failed
        // one-shot settles failed (the cron rule); recurring schedules
        // re-arm; webhooks park for the next delivery.
        let recurring = spec.kind == ka_agent_core::TriggerKind::Schedule
            && spec.recurrence_ms.is_some_and(|rec| rec > 0);
        if recurring {
            let now_zoned = jiff::Timestamp::now().to_zoned(self.cfg().time_zone());
            match crate::runtime::scheduler::trigger_next_after(
                &spec,
                spec.recurrence_ms.unwrap_or(0),
                &now_zoned,
            ) {
                Some(next) => {
                    let _ = self.sh.store.rearm_task(&task.id, next);
                }
                None => {
                    let _ = self.sh.store.settle_task(&task.id, "failed");
                }
            }
        } else if spec.kind == ka_agent_core::TriggerKind::Webhook {
            let _ = self.sh.store.park_trigger(&task.id);
        } else {
            let _ = self.sh.store.settle_task(&task.id, "failed");
        }
    }

    /// Open one worker on the lineage session, submit, drive the events to
    /// completion, and record the turn row. Returns `(stop, cost, tokens,
    /// answer)` on a settled cycle, or the operator-facing failure.
    #[allow(clippy::too_many_arguments)]
    async fn run_trigger_turn(
        &self,
        task: &ka_agent_core::Task,
        who: &Principal,
        lineage_sid: &ka_agent_core::SessionId,
        delivered: Option<&ka_agent_core::QueueItem>,
        inbound: &str,
        wire: &str,
        model: Option<String>,
        allowed_tools: Option<Vec<String>>,
        started_ms: i64,
    ) -> Result<(String, f64, u64, String), String> {
        // Foreign-key order: the lineage session row is created with the
        // trigger; a hand-crafted payload pointing at an unknown session
        // gets one reconstructed here (workspace only — the strand binds
        // on first fire).
        if self.sh.store.get_session(lineage_sid).is_err() {
            let mut session = ka_agent_core::Session::new_for_user(
                self.workspace_for_user(&who.username, &who.role),
                &who.username,
                &who.role,
            );
            session.id = lineage_sid.clone();
            session.created_ms = started_ms;
            session.updated_ms = started_ms;
            self.sh
                .store
                .insert_session(&session)
                .map_err(|e| format!("lineage session: {e}"))?;
        }
        // The creator's partitioning (HOME, data root, grants, keys) with
        // the lineage strand: continuity across fires, nothing of the
        // live chat.
        let cfg = self.cfg();
        let state = self.ensure_user(who).map_err(|e| e.to_string())?;
        let (mut req, _parent_session, _private) = self
            .prepare_open(who, &state, &cfg)
            .map_err(|e| e.to_string())?;
        let lineage = self
            .sh
            .store
            .get_session(lineage_sid)
            .map_err(|e| e.to_string())?;
        req.workspace = lineage.workspace.clone();
        req.strand = match (&lineage.strand_path, lineage.strand_id.as_ref()) {
            (Some(p), Some(_)) if p.exists() => ka_agent_port::StrandSelector::Path(p.clone()),
            _ => ka_agent_port::StrandSelector::New,
        };
        // The turn is durable before the submit handshake, exactly like the
        // drain path (gap G8): a crash leaves a recorded interruption.
        let mut turn = Turn::new(lineage_sid.clone(), inbound.to_string());
        turn.model = model.clone();
        self.sh
            .store
            .insert_turn(&turn)
            .map_err(|e| e.to_string())?;
        let _ = self
            .sh
            .store
            .set_turn_origin(&turn.id, &crate::runtime::scheduler::trigger_source(task));
        if let Some(item) = delivered {
            let _ = self.sh.store.mark_turned(&item.id, &turn.id);
        }
        let port = self.sh.port.open(req).await.map_err(|e| e.to_string())?;
        if let Err(e) = port
            .submit_full(
                wire.to_string(),
                Vec::new(),
                Vec::new(),
                model,
                allowed_tools,
            )
            .await
        {
            port.kill().await;
            let _ = turn.transition(ka_agent_core::TurnStatus::Interrupted);
            self.sh.store.update_turn(&turn).ok();
            return Err(format!("submit: {e}"));
        }
        turn.transition(ka_agent_core::TurnStatus::Submitted)
            .map_err(|t| format!("illegal turn state {t:?}"))?;
        self.sh.store.update_turn(&turn).ok();
        turn.transition(ka_agent_core::TurnStatus::Running)
            .map_err(|t| format!("illegal turn state {t:?}"))?;
        self.sh.store.update_turn(&turn).ok();

        // Drive the cycle: the watchdog applies the same silence rules as a
        // chat turn (`turn_watchdog`); asks are denied fail-closed (an
        // automation never widens its permissions).
        let abort_secs = cfg.turn_watchdog.abort_secs;
        let mut rx = port.take_events();
        let mut answer = String::new();
        let mut stop = String::new();
        let mut cost_usd = 0.0f64;
        let mut tokens = 0u64;
        let mut engine_error: Option<String> = None;
        let mut closed = false;
        let mut last_activity = std::time::Instant::now();
        loop {
            let wait = if abort_secs > 0 {
                std::time::Duration::from_millis(1000)
            } else {
                std::time::Duration::from_secs(3600)
            };
            let ev = tokio::select! {
                ev = rx.recv() => ev,
                _ = tokio::time::sleep(wait) => {
                    if abort_secs > 0
                        && last_activity.elapsed()
                            >= std::time::Duration::from_secs(u64::from(abort_secs))
                    {
                        port.kill().await;
                        closed = true;
                        break;
                    }
                    continue;
                }
            };
            match ev {
                Some(PortEvent::DeltaText(delta)) => {
                    answer.push_str(&delta);
                    last_activity = std::time::Instant::now();
                }
                Some(PortEvent::Bootstrap {
                    strand_id,
                    strand_path,
                    ..
                }) => {
                    // Session lineage: bind the strand so the next fire
                    // resumes this transcript.
                    if let (Some(id), Some(p)) = (strand_id.as_deref(), strand_path.as_deref()) {
                        let _ = self
                            .sh
                            .store
                            .set_strand(lineage_sid, id, std::path::Path::new(p));
                    }
                    last_activity = std::time::Instant::now();
                }
                Some(PortEvent::Ask {
                    ask_id, question, ..
                }) => {
                    port.answer(ask_id, ApprovalDecision::Deny.ka_choice())
                        .await
                        .ok();
                    let mut rec = AuditRecord::new(
                        "runtime",
                        audit_actions::APPROVAL_DECIDE,
                        task.id.as_str(),
                        Some(format!(
                            "deny: a trigger turn may not ask ({})",
                            question.chars().take(120).collect::<String>()
                        )),
                    )
                    .with_user(&who.username)
                    .with_role(&who.role);
                    let _ = self.sh.store.append_audit(&mut rec);
                    last_activity = std::time::Instant::now();
                }
                Some(PortEvent::TurnFinished {
                    stop: word,
                    cost_usd: cost,
                    tokens: count,
                }) => {
                    stop = word;
                    cost_usd = cost;
                    tokens = count;
                    last_activity = std::time::Instant::now();
                }
                Some(PortEvent::Idle) => break,
                Some(PortEvent::EngineClosed) => {
                    closed = true;
                    break;
                }
                Some(PortEvent::EngineError { class, message, .. }) => {
                    engine_error = Some(format!("({class}) {message}"));
                    last_activity = std::time::Instant::now();
                }
                Some(_) => {}
                None => {
                    closed = true;
                    break;
                }
            }
        }
        // Settle the turn from what actually came back (the engine may
        // report a failed optional component and still finish cleanly).
        let settled = stop == "done" || stop == "length";
        let next_status = if closed && stop.is_empty() {
            ka_agent_core::TurnStatus::Interrupted
        } else {
            match stop.as_str() {
                "done" | "length" => ka_agent_core::TurnStatus::Done,
                "aborted" => ka_agent_core::TurnStatus::Aborted,
                _ => ka_agent_core::TurnStatus::Error,
            }
        };
        turn.cost_usd = cost_usd;
        turn.tokens = tokens;
        if !settled {
            turn.error_class = Some("trigger".into());
            turn.error_message = engine_error.clone();
        }
        turn.transition(next_status)
            .map_err(|t| format!("illegal turn state {t:?}"))?;
        self.sh.store.update_turn(&turn).ok();
        if settled {
            port.close().await.ok();
            Ok((stop, cost_usd, tokens, answer))
        } else {
            port.kill().await;
            let why = if stop.is_empty() {
                engine_error.unwrap_or_else(|| "engine closed before finishing".to_string())
            } else if stop == "aborted" {
                "aborted".to_string()
            } else {
                engine_error.unwrap_or_else(|| format!("engine reported {stop}"))
            };
            Err(why)
        }
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
                SessionStatus::Running
                | SessionStatus::AwaitingApproval
                | SessionStatus::Stopping => false,
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
                SessionStatus::Running => {
                    let live = self.live_port_for(who);
                    // A3: a bare interrupt word ("stop", "wait", …) aborts
                    // the live cycle and the word itself becomes the next
                    // turn (the `/stop` abort shape; the drain submits it
                    // at the turn boundary). `interrupts = "off"` keeps the
                    // steer/queue split below untouched.
                    if self.cfg().interrupts == "on"
                        && live.is_some()
                        && is_bare_interrupt(text)
                        && self.abort_live_cycle(st).await
                    {
                        self.sh.emit_from(
                            AgentEvent::Notice {
                                text: "⏹ stopped — your message is next".into(),
                            },
                            Some(source),
                        );
                        let sid = st.lock().session.id.clone();
                        let item = QueueItem::new(sid, text.to_string(), source);
                        self.sh.store.enqueue_front(&item)?;
                        let n = self.sh.store.queue_len(&st.lock().session.id)?;
                        self.sh.emit(AgentEvent::Queue { depth: n as u32 });
                        return Ok(());
                    }
                    // `busy_messages = "steer"`: an operator message lands in
                    // the running reply at the next tool-batch boundary.
                    // Scheduled turns stay uninterrupted.
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
                            let _ = self.sh.store.append_turn_inbound(
                                id,
                                &format!("\n\n[added while working] {text}"),
                            );
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
                    let item =
                        QueueItem::new(st.lock().session.id.clone(), text.to_string(), source);
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
                SessionStatus::AwaitingApproval | SessionStatus::Stopping => {
                    // No running reply to steer into (the engine is
                    // blocked on a card, or a stop is in flight): the
                    // message queues durably and drains when the
                    // boundary closes. Steering here used to hand the
                    // text to an idle engine that dropped it.
                    let item =
                        QueueItem::new(st.lock().session.id.clone(), text.to_string(), source);
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
            .submit_text_with(text, Vec::new(), Some(source), who, tool_override, false)
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
        let skills = crate::skills::Skills::new(&self.sh.paths.user_engine_home(&who.username));
        if !skills.is_enabled(name)? {
            return Err(Error::Refused(format!(
                "skill {name:?} is not enabled — `ka-agent skills enable {name}`"
            )));
        }
        if text.trim().is_empty() {
            return Err(Error::Refused("usage: /skill <name> <text>".into()));
        }
        // Turn-scoped tools from the operator's source copy (the engine
        // copy is stripped). Never widens: `submit_text_with` intersects
        // this with the role's toolset.
        let tool_override = skills
            .resolved()
            .into_iter()
            .find(|skill| skill.info.name == name)
            .map(|skill| skill.info.allowed_tools)
            .filter(|tools| !tools.is_empty());
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
            .submit_text_with(
                text,
                vec![name.to_string()],
                Some(source),
                who,
                tool_override,
                true,
            )
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
        explicit_skill: bool,
    ) -> Result<(), Error> {
        let st = self.ensure_user(who)?;
        // The bridge reads the turn's source, user and role from the store
        // while the turn runs.
        publish_turn_context(&self.sh.store, source.unwrap_or("console"), who);
        if let Err(msg) = self.preflight(who) {
            self.sh.emit_from(AgentEvent::Notice { text: msg }, source);
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
                let item =
                    QueueItem::new(sid.clone(), text.to_string(), source.unwrap_or("console"));
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
            self.sh.emit_from(AgentEvent::Notice { text: e }, source);
            None
        });
        // A private conversation records usage only: the row carries no text.
        let mut turn = Turn::new(
            sid.clone(),
            if private {
                String::new()
            } else {
                text.to_string()
            },
        );
        turn.model = model.clone();
        self.sh.store.insert_turn(&turn)?;
        let _ = self
            .sh
            .store
            .set_turn_origin(&turn.id, source.unwrap_or("console"));
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
                live.turn_from_nudge = false;
                live.thought_tail.clear();
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
        // Trigger hits and the paths skills parked by this conversation.
        // Explicit `/skill` names stay in front of the 3-skill cap.
        let attach = plan_skill_attach(
            &self.sh,
            who,
            &st,
            sid.as_str(),
            text,
            explicit_skill.then_some(skills_extra.as_slice()),
            !is_scheduled(source),
        );
        skills.extend(skills_extra);
        for name in &attach.attach {
            if !skills.contains(name) {
                skills.push(name.clone());
            }
        }
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
        let recall = self
            .recall_block_for(who, source.unwrap_or("console"), text)
            .unwrap_or_default();
        let wire = format!(
            "{}{}\n{}{text}",
            context_line(&self.sh, source.unwrap_or("console")),
            agent_preamble,
            recall
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
        let allowed_tools = narrow_tools(role_allow, tool_override);
        // B15b: a scheduled turn runs against the automation compile
        // (machine facts only); every exit of this bracket restores the
        // full one — Ok at TurnFinished/Idle (and immediately below for
        // a fast scripted finish), Err right in the arms.
        let scheduled = is_scheduled(source);
        if scheduled && let Err(e) = self.install_automation_memory_for(&who.username) {
            self.sh.emit(AgentEvent::Error {
                class: "internal".into(),
                retryable: retryable_class("internal"),
                message: format!("automation memory compile: {e}"),
            });
        }
        match port
            .submit_full(wire, Vec::new(), skills, model, allowed_tools)
            .await
        {
            Ok(()) => record_skill_attach(&self.sh, who, &st, sid.as_str(), &attach),
            Err(PortError::EngineClosed) => {
                turn.transition(TurnStatus::Interrupted)
                    .map_err(|t| illegal_turn(turn.status, t))?;
                self.sh.store.update_turn(&turn)?;
                rt_on_engine_closed(&self.sh, &st, port).await;
                if scheduled && let Err(e) = self.refresh_compiled_memory_for(&who.username) {
                    self.sh.emit(AgentEvent::Error {
                        class: "internal".into(),
                        retryable: retryable_class("internal"),
                        message: format!("memory restore: {e}"),
                    });
                }
                return Ok(());
            }
            Err(e) => {
                if scheduled && let Err(why) = self.refresh_compiled_memory_for(&who.username) {
                    self.sh.emit(AgentEvent::Error {
                        class: "internal".into(),
                        retryable: retryable_class("internal"),
                        message: format!("memory restore: {why}"),
                    });
                }
                return Err(e.into());
            }
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
            if scheduled {
                self.refresh_compiled_memory_for(&who.username).ok();
            }
            return Ok(());
        }
        turn.transition(TurnStatus::Running)
            .map_err(|t| illegal_turn(turn.status, t))?;
        self.sh.store.update_turn(&turn)?;
        Ok(())
    }

    /// `/stop`'s abort shape without its notices: flip a live cycle to
    /// `Stopping`, send `port.abort()`, and arm the grace escalation on
    /// the *turn* (a successful abort leaves the worker attached). `true`
    /// when an abort was sent. Shared by `/stop`, the interrupt keywords
    /// and `/queue now` (single-writer: none of them submits a mid-cycle
    /// prompt — the next cycle is simply the next `drain_queue` head).
    pub(super) async fn abort_live_cycle(&self, st: &UserHandle) -> bool {
        let port = {
            let mut g = st.lock();
            let status = g.session.status;
            let live_port = g.live.as_ref().map(|l| Arc::clone(&l.port));
            match (status, live_port) {
                (SessionStatus::Running | SessionStatus::AwaitingApproval, Some(p)) => {
                    // The edge is legal by construction (same lock scope).
                    if g.session.transition(SessionStatus::Stopping).is_ok() {
                        Some(p)
                    } else {
                        None
                    }
                }
                _ => None,
            }
        };
        let Some(port) = port else {
            return false;
        };
        port.abort().await.ok();
        let grace = Duration::from_secs(self.cfg().turn_grace_secs);
        // Escalate on the *turn*, not the process (SEED.md §3.8).
        let sh = Arc::clone(&self.sh);
        let st = st.clone();
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
        true
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

    /// The firing view (`/skill-doctor`): enabled names joined with the
    /// operator library's triggers/paths/allowed_tools.
    pub fn skills_resolved(&self, who: &Principal) -> Vec<crate::skills::ResolvedSkill> {
        crate::skills::Skills::new(&self.sh.paths.user_engine_home(&who.username)).resolved()
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
    fn skill_tools_never_widen() {
        let role = Some(vec!["read".to_string()]);
        let skill = Some(vec!["read".to_string(), "bash".to_string()]);
        // A skill asking for bash cannot widen a role that only gets read.
        assert_eq!(narrow_tools(role.clone(), skill), role);
        // No role allowlist (admin) accepts the skill's list as written.
        assert_eq!(
            narrow_tools(None, Some(vec!["read".to_string(), "bash".to_string()])),
            Some(vec!["read".to_string(), "bash".to_string()])
        );
        // No override keeps the role toolset untouched.
        assert_eq!(narrow_tools(role.clone(), None), role);
        assert_eq!(narrow_tools(None, None), None);
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
    fn bare_interrupt_words_match_alone_only() {
        // Every high word, any case, trailing punctuation tolerated.
        for word in [
            "stop", "halt", "cease", "quit", "end", "abort", "cancel", "wait", "hold", "pause",
        ] {
            assert!(is_bare_interrupt(word), "{word}");
            let loud = format!("{}!", word.to_uppercase());
            assert!(is_bare_interrupt(&loud), "{loud}");
            assert!(is_bare_interrupt(&format!(" {word}. ")), "'{word}.'");
            assert!(is_bare_interrupt(&format!("{word}?")), "{word}?");
        }
        // Benign shorts never match — the medium words ride the steer path.
        for benign in [
            "ok",
            "ok.",
            "thanks",
            "hm",
            "no",
            "wrong",
            "actually",
            "nevermind",
            "go on",
        ] {
            assert!(!is_bare_interrupt(benign), "{benign}");
        }
        // Exactly one word: sentences — even short ones — do not match.
        assert!(!is_bare_interrupt("stop now"));
        assert!(!is_bare_interrupt("please stop"));
        // The 20-char cap: a long sentence with an interrupt word is a
        // message, not an interrupt.
        assert!(!is_bare_interrupt("wait for the green build"));
        assert!(!is_bare_interrupt(""));
        assert!(!is_bare_interrupt("   "));
        assert!(!is_bare_interrupt("?!"));
        // The ≤20-char cap: anything longer is an ordinary message even
        // when it is a single word-shaped run.
        let long_word = format!("sto{}p", "o".repeat(18)); // 22 chars
        assert!(!is_bare_interrupt(&long_word));
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
