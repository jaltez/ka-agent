// Event pump, one per live worker: port events -> that user's state + store +
// surface, turn lifecycle, final-answer chunking. Split from runtime.rs
// (moves only).
use super::approvals::session_allow_key;
use super::intake::drain_queue;
use super::policy::{self, AskContext};
use super::worker::rt_on_engine_closed;
use super::*;
use crate::event::{PlanItem, turn_text};
use crate::surface::ApprovalCard;
use ka_agent_core::Principal;
use ka_agent_store::{PlanRow, PlanStatus};

/// Final-answer chunk ceiling before splitting (Telegram 4096 minus margin;
/// SEED.md §5.4).
pub const FINAL_CHUNK_MAX: usize = 3800;

/// Is this turn one a schedule started (cron / briefing)?
pub(crate) fn is_scheduled(source: Option<&str>) -> bool {
    source.is_some_and(ka_agent_core::is_scheduled_source)
}

/// Split a final answer into channel chunks at paragraph boundaries,
/// hard-splitting pathological single paragraphs.
pub fn split_final_text(text: &str, max: usize) -> Vec<String> {
    let max = max.max(1);
    if text.chars().count() <= max {
        return vec![text.to_string()];
    }
    let mut out = Vec::new();
    let mut cur = String::new();
    for para in text.split("\n\n") {
        // Paragraphs are never merged across a cap boundary.
        if !cur.is_empty() && cur.chars().count() + 2 + para.chars().count() > max {
            out.push(std::mem::take(&mut cur));
        }
        if para.chars().count() > max {
            // Pathological single paragraph: hard-split, keep the tail.
            if !cur.is_empty() {
                out.push(std::mem::take(&mut cur));
            }
            // One pass over the char boundaries instead of re-counting and
            // re-collecting the tail per chunk (O(n) vs O(n²) on a
            // megabyte paragraph).
            let bounds: Vec<usize> = para
                .char_indices()
                .map(|(i, _)| i)
                .chain(std::iter::once(para.len()))
                .collect();
            let n = bounds.len() - 1;
            let mut lo = 0;
            while lo + max < n {
                out.push(para[bounds[lo]..bounds[lo + max]].to_string());
                lo += max;
            }
            cur.push_str(&para[bounds[lo]..]);
        } else {
            if !cur.is_empty() {
                cur.push_str("\n\n");
            }
            cur.push_str(para);
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    if out.is_empty() {
        out.push(String::new());
    }
    out
}

/// The engine phrases every gated call as "run `<command>`?"; tools without
/// a command (MCP servers, bridge tools) come out as "run ``?". Name the
/// call instead so the operator knows what they are allowing.
fn readable_question(question: &str, last_call: Option<&(String, String)>) -> String {
    match last_call {
        Some((tool, detail)) if question.contains("``") => {
            let detail = detail.trim();
            if detail.is_empty() {
                format!("use `{tool}`?")
            } else {
                format!("use `{tool}` ({detail})?")
            }
        }
        _ => question.to_string(),
    }
}

/// The model's last notes for an empty-final give-up: the final
/// ~400 characters of the reasoning tail with whitespace collapsed to
/// single spaces (it rides inside one message, not as a wall of log
/// lines). Empty when the turn left no reasoning at all.
fn thought_excerpt(tail: &str) -> String {
    let collapsed: String = tail.split_whitespace().collect::<Vec<_>>().join(" ");
    let chars: Vec<char> = collapsed.chars().collect();
    let start = chars.len().saturating_sub(400);
    chars[start..].iter().collect()
}

/// Return the user's session to `idle` once nothing is left in flight: the
/// turn's record is terminal (or no turn is recorded and no prompt is being
/// handed over), no ask is pending and the queue is empty. Called from both
/// the `turn_finished` and `idle` handlers.
pub(super) fn rt_maybe_idle(sh: &Arc<Shared>, st: &UserHandle) {
    let (sid, status, ask_pending, current) = {
        let g = st.lock();
        (
            g.session.id.clone(),
            g.session.status,
            g.live.as_ref().is_some_and(|l| !l.pending_asks.is_empty()),
            g.live.as_ref().and_then(|l| l.current_turn.clone()),
        )
    };
    if status == SessionStatus::Idle || ask_pending {
        return;
    }
    // No turn recorded, but an inbound message set the session `running`: its
    // prompt is still being handed over, so the engine's `idle` is not the
    // end of it.
    let turn_open = match current {
        Some(tid) => sh
            .store
            .get_turn(&tid)
            .ok()
            .flatten()
            .is_some_and(|t| !t.status.is_terminal()),
        None => status == SessionStatus::Running,
    };
    let queued = sh.store.queue_len(&sid).unwrap_or(0);
    if turn_open || queued > 0 {
        return;
    }
    let mut g = st.lock();
    let from = g.session.status;
    g.session
        .transition(SessionStatus::Idle)
        .map_err(|t| illegal_session(from, t))
        .ok();
    let status = g.session.status;
    drop(g);
    rt_persist_status(sh, st);
    sh.emit(AgentEvent::Session {
        status: format!("{status:?}"),
    });
}

pub(super) fn rt_persist_status(sh: &Arc<Shared>, st: &UserHandle) {
    let (sid, status) = {
        let g = st.lock();
        (g.session.id.clone(), g.session.status)
    };
    if let Err(e) = sh.store.update_session_status(&sid, status) {
        sh.emit(AgentEvent::Error {
            class: "internal".into(),
            retryable: retryable_class("internal"),
            message: format!("session status persist: {e}"),
        });
    }
}

/// Which runtime turn's start has already been announced.
///
/// One `submit → … → idle` cycle is one runtime turn, but the engine emits
/// `turn_started` for every engine turn inside it (auto-poke, verify, ka's
/// own queued input). Surfaces reset their per-turn stream on
/// [`AgentEvent::TurnStarted`], so only the first start of each runtime turn
/// may be announced.
#[derive(Default)]
struct TurnAnnouncements(Option<String>);

impl TurnAnnouncements {
    /// `true` when this port `turn_started` opens a runtime turn that has
    /// not been announced yet.
    fn admit(&mut self, turn: &str) -> bool {
        if self.0.as_deref() == Some(turn) {
            return false;
        }
        self.0 = Some(turn.to_string());
        true
    }
}

/// The per-worker event loop: maps port events onto one user's state, store
/// and surface (SEED.md §3.7: only events after our own submission are live
/// output; the delivery boundary is `idle`).
pub(super) async fn drive(
    sh: Arc<Shared>,
    who: Principal,
    st: UserHandle,
    port: Arc<PortSession>,
    mut rx: mpsc::Receiver<PortEvent>,
) {
    // Context meter dedupe: emit only when the integer percentage moves.
    let mut last_ctx_pct: Option<u64> = None;
    // The runtime turn whose start was already announced.
    let mut announcements = TurnAnnouncements::default();
    while let Some(ev) = rx.recv().await {
        // Port identity: a superseded worker (`/new`, idle reap, cap
        // eviction) can still flush events while its kill is in flight. They
        // belong to a session that is no longer live — drop them instead of
        // folding them into the current one.
        let is_current = {
            let g = st.lock();
            g.live
                .as_ref()
                .is_some_and(|live| Arc::ptr_eq(&live.port, &port))
        };
        if !is_current {
            continue;
        }
        // Turn-scoped events carry the source they belong to: surfaces with
        // per-identity delivery route replies to the originating chat.
        let turn_source = {
            let g = st.lock();
            g.live.as_ref().and_then(|l| l.current_turn_source.clone())
        };
        let emit = |ev: AgentEvent| sh.emit_from(ev, turn_source.as_deref());
        // Any event is activity: the idle reap's clock resets.
        {
            let mut g = st.lock();
            if let Some(live) = g.live.as_mut() {
                live.last_activity_ms = ka_agent_core::now_ms();
                live.watchdog_notified = false;
            }
            g.last_active_ms = ka_agent_core::now_ms();
        }
        match ev {
            PortEvent::ModelChanged(selector) => {
                // The engine is answering with a different model than the
                // configured one (a fallback hop): remember it for
                // status/doctor and tell surfaces (F3).
                *sh.live_model.lock().unwrap_or_else(|e| e.into_inner()) = Some(selector.clone());
                emit(AgentEvent::Settings {
                    model: Some(selector),
                    mode: None,
                    effort: None,
                    private: None,
                });
            }
            PortEvent::Bootstrap {
                strand_id,
                strand_path,
                replay_messages,
            } => {
                let mut g = st.lock();
                if let Some(id) = strand_id {
                    g.session.strand_id = Some(id);
                }
                if let Some(p) = strand_path {
                    g.session.strand_path = Some(std::path::PathBuf::from(p));
                }
                g.session.updated_ms = ka_agent_core::now_ms();
                let snapshot = g.session.clone();
                drop(g);
                if let (Some(sid_ref), Some(path_ref)) =
                    (&snapshot.strand_id, &snapshot.strand_path)
                    && let Err(e) = sh.store.set_strand(&snapshot.id, sid_ref, path_ref)
                {
                    emit(AgentEvent::Error {
                        class: "internal".into(),
                        retryable: retryable_class("internal"),
                        message: format!("strand persist: {e}"),
                    });
                }
                if replay_messages > 0 {
                    emit(AgentEvent::Note {
                        text: format!(
                            "resumed session: {replay_messages} replayed messages (not re-delivered)"
                        ),
                    });
                }
            }
            PortEvent::TurnStarted => {
                let (turn, source) = {
                    let g = st.lock();
                    (
                        g.live
                            .as_ref()
                            .and_then(|l| l.current_turn.clone())
                            .map(|t| t.to_string())
                            .unwrap_or_default(),
                        g.live
                            .as_ref()
                            .and_then(|l| l.current_turn_source.clone())
                            .unwrap_or_default(),
                    )
                };
                if !announcements.admit(&turn) {
                    continue;
                }
                emit(AgentEvent::TurnStarted { turn, source });
            }
            PortEvent::DeltaText(t) => {
                let scheduled = {
                    let mut g = st.lock();
                    let mut scheduled = false;
                    if let Some(live) = g.live.as_mut() {
                        live.live_text.push_str(&t);
                        scheduled = is_scheduled(live.current_turn_source.as_deref());
                    }
                    scheduled
                };
                // Scheduled turns stay silent while running: the answer is
                // delivered as one chunk at the boundary (or not at all,
                // when it is `[SILENT]`). Tool events still flow.
                if !scheduled {
                    emit(AgentEvent::Delta { text: t });
                }
            }
            PortEvent::DeltaThought(t) => {
                let scheduled = {
                    let mut g = st.lock();
                    let mut scheduled = false;
                    if let Some(live) = g.live.as_mut() {
                        // Keep the reasoning's tail: an empty-final
                        // give-up quotes it as the model's last notes.
                        live.thought_tail.push_str(&t);
                        let len = live.thought_tail.chars().count();
                        if len > THOUGHT_TAIL_MAX {
                            let skip = len - THOUGHT_TAIL_MAX;
                            live.thought_tail = live.thought_tail.chars().skip(skip).collect();
                        }
                        scheduled = is_scheduled(live.current_turn_source.as_deref());
                    }
                    scheduled
                };
                if !scheduled {
                    emit(AgentEvent::Thought { text: t });
                }
            }
            PortEvent::CallStarted { tool, id, detail } => {
                // B12 paths: a touch parks the skill for the *next*
                // submit (single-writer — never mid-cycle). The
                // once-per-conversation marker is written at attach time,
                // so a touch whose marker is still set is a no-op.
                let sid = st.lock().session.id.clone();
                if !is_scheduled(turn_source.as_deref()) {
                    let resolved =
                        crate::skills::Skills::new(&sh.paths.user_engine_home(&who.username))
                            .resolved();
                    let hits = crate::skills::matched_paths(&detail, &resolved);
                    // A name already parked (or attached, marker set) is a
                    // no-op: the once-per-conversation rule.
                    let pending = st.lock().paths_pending.clone();
                    let mut park = Vec::new();
                    for name in hits {
                        if !pending.contains(&name)
                            && !crate::skills::path_marked(&sh.store, sid.as_str(), &name)
                        {
                            park.push(name);
                        }
                    }
                    if !park.is_empty() {
                        st.lock().paths_pending.extend(park);
                    }
                }
                let mut g = st.lock();
                if let Some(live) = g.live.as_mut() {
                    live.last_call = Some((tool.clone(), detail.clone()));
                    live.tool_started.insert(id.clone(), Instant::now());
                }
                drop(g);
                emit(AgentEvent::ToolStarted { id, tool, detail });
            }
            PortEvent::CallFinished { tool, id, ok } => {
                let started = {
                    let mut g = st.lock();
                    g.live
                        .as_mut()
                        .and_then(|live| live.tool_started.remove(&id))
                };
                let excerpt = {
                    let g = st.lock();
                    g.live
                        .as_ref()
                        .and_then(|live| live.tool_excerpt.get(&id))
                        .cloned()
                };
                let ms = started.map_or(0, |t| t.elapsed().as_millis() as u64);
                let error = if ok {
                    None
                } else {
                    excerpt
                        .as_deref()
                        .and_then(|e| e.lines().next())
                        .map(str::to_string)
                };
                let output = excerpt.clone().filter(|_| ok);
                emit(AgentEvent::ToolFinished {
                    id,
                    tool,
                    ok,
                    ms,
                    error,
                    output,
                });
            }
            PortEvent::CallOutput { excerpt, id, .. } => {
                // Keep the last excerpt per call: a failed call's first
                // line rides its `ToolFinished`, and a successful call's
                // excerpt rides it too.
                let capped: String = excerpt.chars().take(2000).collect();
                let mut g = st.lock();
                if let Some(live) = g.live.as_mut() {
                    live.tool_excerpt.insert(id, capped);
                }
            }
            PortEvent::Plan(items) => {
                emit(AgentEvent::Plan {
                    items: items
                        .into_iter()
                        .map(|(text, state)| PlanItem { text, state })
                        .collect(),
                });
            }
            PortEvent::Context { used, window } => {
                let pct = (used * 100).checked_div(window.max(1)).unwrap_or(0);
                if last_ctx_pct != Some(pct) {
                    last_ctx_pct = Some(pct);
                    emit(AgentEvent::Context { used, window });
                }
            }
            PortEvent::Title(title) => {
                if !title.is_empty() {
                    let mut g = st.lock();
                    if let Some(live) = g.live.as_mut() {
                        live.title = Some(title);
                    }
                }
            }
            PortEvent::Compacting { done } => {
                // B12: compaction resets the session's paths markers so the
                // next matching touch re-parks. Lazy re-attach only — no
                // eager re-injection across the port.
                if done {
                    let sid = st.lock().session.id.clone();
                    crate::skills::clear_path_markers(
                        &sh.store,
                        sid.as_str(),
                        &sh.paths.user_engine_home(&who.username),
                    );
                }
                emit(AgentEvent::Compacting { done });
            }
            PortEvent::Inventory { tools } => {
                let mut g = st.lock();
                if let Some(live) = g.live.as_mut() {
                    live.tools = tools;
                }
            }
            PortEvent::Ask {
                ask_id,
                question,
                detail,
                options,
            } => {
                let (sid, turn_id, ttl, last_call) = {
                    let g = st.lock();
                    (
                        g.session.id.clone(),
                        g.live
                            .as_ref()
                            .and_then(|l| l.current_turn.clone())
                            .unwrap_or_else(TurnId::generate),
                        cfg_of(&sh).approval_ttl_secs,
                        g.live.as_ref().and_then(|l| l.last_call.clone()),
                    )
                };
                let question = readable_question(&question, last_call.as_ref());
                // Allowed for this session already: answer at once, no card
                // (still audited).
                let ask_detail = detail.clone();
                let key = session_allow_key(&question, ask_detail.as_deref());
                let pre_allowed = st.lock().session_allow.contains(&key);
                if pre_allowed {
                    // The one Allow settle site (B7): the session-allow
                    // set answers before any card exists — `answer_allow`
                    // with no approval row.
                    let rt = Runtime {
                        sh: Arc::clone(&sh),
                    };
                    if rt
                        .answer_allow(
                            &st,
                            &who,
                            None,
                            ask_id.clone(),
                            "Allow (session)",
                            "operator",
                        )
                        .await
                        .is_ok()
                    {
                        emit(AgentEvent::Note {
                            text: format!("✓ allowed for this session: {question}"),
                        });
                    }
                    continue;
                }
                let kind = ApprovalKind::KaAsk {
                    ask_id: ask_id.clone(),
                    question: question.clone(),
                    detail: detail.clone(),
                    options: options.clone(),
                };
                // D9: self-scope stays on the acting user's surface;
                // anything shared, over quota or listed in `escalate` goes to
                // the admin queue with the reason on the card.
                let spec = (who.role != ka_agent_core::ADMIN_ROLE).then(|| {
                    Runtime {
                        sh: Arc::clone(&sh),
                    }
                    .effective_role(&who.role)
                });
                let budget_exceeded = st.lock().budget.verdict() == BudgetVerdict::Exceeded;
                let user_root = sh.paths.user_root(&who.username);
                let ctx = AskContext {
                    role: &who.role,
                    spec: spec.as_ref(),
                    user_root: &user_root,
                    budget_exceeded,
                    tool: last_call.as_ref().map(|(t, _)| t.as_str()),
                    question: &question,
                    detail: detail.as_deref(),
                };
                let routing = policy::route(&ctx);
                let escalate_reason = routing.err();
                let escalated = escalate_reason.is_some();
                let mut approval = Approval::new(sid, turn_id, kind, ttl as i64)
                    .routed_to(&who.username, escalated);
                if let Some(reason) = &escalate_reason {
                    // The reason rides the card: the admin decides knowing
                    // why it did not stay with the user.
                    approval.kind = ApprovalKind::KaAsk {
                        ask_id,
                        question: format!("{question}\n[escalated: {reason}]"),
                        detail,
                        options,
                    };
                }
                let card = ApprovalCard::from_approval(&approval);
                if let Err(e) = sh.store.insert_approval(&approval) {
                    emit(AgentEvent::Error {
                        class: "internal".into(),
                        retryable: retryable_class("internal"),
                        message: format!("approval persist: {e}"),
                    });
                }
                // An ask is only actionable while its turn is live.
                let (awaiting, live_port) = {
                    let mut g = st.lock();
                    let from = g.session.status;
                    let awaiting = g
                        .session
                        .transition(SessionStatus::AwaitingApproval)
                        .map_err(|t| illegal_session(from, t))
                        .is_ok();
                    if awaiting && let Some(live) = g.live.as_mut() {
                        live.pending_asks
                            .insert(approval.id.clone(), approval_ask_id(&approval));
                    }
                    let port = g.live.as_ref().map(|l| Arc::clone(&l.port));
                    drop(g);
                    (awaiting, port)
                };
                if awaiting {
                    // Durable rules (v4 B7): evaluated AFTER D9 routing and
                    // the awaiting transition, and only on non-escalated
                    // asks — a rule answers exactly the asks the acting
                    // user themselves decides, never the admin queue's.
                    if !escalated {
                        let rt = Runtime {
                            sh: Arc::clone(&sh),
                        };
                        let mode = rt.effective_mode_for(&who.username);
                        let hit = sh
                            .store
                            .rules_for_subject(&who.username)
                            .unwrap_or_default()
                            .into_iter()
                            .find(|r| {
                                ka_agent_core::config::mode_rank(&mode)
                                    .zip(ka_agent_core::config::mode_rank(&r.mode))
                                    .is_some_and(|(cur, born)| cur <= born)
                                    && r.shape.matches(
                                        &question,
                                        ask_detail.as_deref(),
                                        last_call.as_ref().map(|(t, d)| (t.as_str(), d.as_str())),
                                    )
                            });
                        if let Some(rule) = hit {
                            let label = format!("Allow (rule {})", rule.id);
                            let note = format!("✓ rule {}: {question}", rule.shape.label());
                            let ask = approval_ask_id(&approval);
                            if rt
                                .answer_allow(
                                    &st,
                                    &who,
                                    Some((&approval, ApprovalStatus::Approved)),
                                    ask,
                                    &label,
                                    "runtime",
                                )
                                .await
                                .is_ok()
                            {
                                rt_persist_status(&sh, &st);
                                emit(AgentEvent::Note { text: note });
                                continue;
                            }
                        }
                    }
                    rt_persist_status(&sh, &st);
                    if let Some(card) = card {
                        if escalated {
                            // The admin queue: no turn source, so every
                            // operator surface sees it.
                            sh.emit_from(card.to_event(), None);
                            emit(AgentEvent::Notice {
                                text: format!(
                                    "waiting for the operator to approve ({})",
                                    escalate_reason.unwrap_or_default()
                                ),
                            });
                        } else {
                            // The acting user's own card, on their surface.
                            sh.emit_from(card.to_event(), turn_source.as_deref());
                        }
                    }
                    continue;
                }
                if let Some(port) = live_port {
                    let _ = port
                        .answer(
                            approval_ask_id(&approval),
                            ApprovalDecision::Deny.ka_choice(),
                        )
                        .await;
                }
                if let Err(e) = sh.store.decide_approval(
                    &approval.id,
                    ApprovalStatus::Denied,
                    ka_agent_core::now_ms(),
                ) {
                    emit(AgentEvent::Error {
                        class: "internal".into(),
                        retryable: retryable_class("internal"),
                        message: format!("ask deny persist: {e}"),
                    });
                }
                let mut rec = AuditRecord::new(
                    "runtime",
                    audit_actions::APPROVAL_DECIDE,
                    approval.id.as_str(),
                    Some("deny: the turn this ask belongs to already finished".into()),
                )
                .with_user(&who.username)
                .with_role(&who.role);
                let _ = sh.store.append_audit(&mut rec);
                emit(AgentEvent::Notice {
                    text: "ask auto-denied: its turn already finished (nothing to approve)".into(),
                });
                invariants::check_and_report(&sh, &st);
            }
            PortEvent::TurnFinished {
                stop,
                cost_usd,
                tokens,
            } => {
                // A settled turn ends any rate-limit throttle: the stamp
                // only survives while the engine is still retrying.
                st.lock().throttled_since_ms = None;
                let turn_result = { st.lock().live.as_ref().and_then(|l| l.current_turn.clone()) };
                let (full_text, private) = {
                    let g = st.lock();
                    (
                        g.live
                            .as_ref()
                            .map(|l| l.live_text.clone())
                            .unwrap_or_default(),
                        g.session.private,
                    )
                };
                // A done/length turn whose whole cycle streamed not one
                // character of answer text (a reasoning-only reply:
                // content empty, everything in thinking) is recorded with
                // the stop word `empty` instead of `done` — the silent
                // nothing-was-delivered family must be distinguishable
                // from a real answer in telemetry and /history, not just
                // cheaper to bill.
                let empty_final =
                    matches!(stop.as_str(), "done" | "length") && full_text.trim().is_empty();
                if let Some(tid) = turn_result.clone()
                    && let Some(mut t) = sh.store.get_turn(&tid).ok().flatten()
                {
                    t.stop = Some(if empty_final {
                        "empty".into()
                    } else {
                        stop.clone()
                    });
                    t.cost_usd = cost_usd;
                    t.tokens = tokens;
                    // The engine can finish a turn before the submit
                    // handshake recorded it as running: walk the record up
                    // so the finish always lands on a legal edge.
                    if t.status == TurnStatus::Submitted {
                        t.transition(TurnStatus::Running).ok();
                        sh.store.update_turn(&t).ok();
                    }
                    let next = match stop.as_str() {
                        "done" | "length" => Some(TurnStatus::Done),
                        "error" => Some(TurnStatus::Error),
                        "aborted" => Some(TurnStatus::Aborted),
                        _ => None,
                    };
                    if let Some(next) = next {
                        t.transition(next)
                            .map_err(|x| illegal_turn(t.status, x))
                            .ok();
                    }
                    if let Err(e) = sh.store.update_turn(&t) {
                        emit(AgentEvent::Error {
                            class: "internal".into(),
                            retryable: retryable_class("internal"),
                            message: format!("turn persist: {e}"),
                        });
                    }
                }
                // An ask outlives nothing: with its turn terminal, no
                // engine is blocked on the card's answer — waiting for
                // the card's TTL only parks the session in
                // `awaiting_approval`, during which inbound messages
                // would steer into a reply that no longer runs and
                // vanish (live finding, 2026-11-17: a `/stop` under an
                // open card silently swallowed the next prompt). Deny
                // the orphans now — same wording and settle path as a
                // late ask — so the boundary can close.
                let orphans: Vec<(ApprovalId, String, Arc<PortSession>)> = {
                    let mut g = st.lock();
                    g.live
                        .as_mut()
                        .map(|live| {
                            live.pending_asks
                                .drain()
                                .map(|(id, ask)| (id, ask, Arc::clone(&live.port)))
                                .collect()
                        })
                        .unwrap_or_default()
                };
                for (id, ask_id, port) in orphans {
                    port.answer(ask_id, ApprovalDecision::Deny.ka_choice())
                        .await
                        .ok();
                    sh.store
                        .decide_approval(&id, ApprovalStatus::Denied, ka_agent_core::now_ms())
                        .ok();
                    let mut rec = AuditRecord::new(
                        "runtime",
                        audit_actions::APPROVAL_DECIDE,
                        id.as_str(),
                        Some("deny: its turn already finished".into()),
                    )
                    .with_user(&who.username)
                    .with_role(&who.role);
                    let _ = sh.store.append_audit(&mut rec);
                    sh.emit(AgentEvent::ApprovalSettled {
                        id: id.as_str().to_string(),
                        status: format!("{:?}", ApprovalStatus::Denied),
                    });
                    emit(AgentEvent::Notice {
                        text: "ask auto-denied: its turn already finished (nothing to approve)"
                            .into(),
                    });
                }
                // Crash insurance (SEED §3.7): one prompt cycle can hold
                // several engine turns (auto-poke, verify) under a single
                // runtime turn, and the delivery boundary — `idle` — only
                // fires after the last one. From the first `turn_finished`
                // the answer so far is durable in the outbox: a crash or
                // restart mid-cycle redelivers it at startup instead of
                // losing turns the store already calls done while their
                // text lived only in the in-memory buffer. The rows are
                // cumulative — every finish rewrites them from the whole
                // buffered text — and `idle` still owns the actual
                // handoff: it re-pushes the final split and marks the rows
                // sent as its `FinalChunk` events go out. A send that is
                // refused later goes to `failed`, which the channel's
                // retry pass redelivers in-run. Private conversations
                // write no rows (their answers never outlive the session;
                // delivery rides the FinalChunk events only).
                if let Some(tid) = turn_result {
                    let silent = full_text.trim_start().starts_with("[SILENT]");
                    if !private && !silent && !full_text.trim().is_empty() {
                        for (i, chunk) in split_final_text(&full_text, FINAL_CHUNK_MAX)
                            .iter()
                            .enumerate()
                        {
                            let oc = OutboxChunk::new(tid.clone(), i as u32, chunk.clone());
                            sh.store.push_chunk(&oc).ok();
                        }
                    }
                }
                let (verdict, budget) = {
                    let mut g = st.lock();
                    g.budget.record(cost_usd, tokens, ka_agent_core::now_ms());
                    (g.budget.verdict(), g.budget.clone())
                };
                Runtime {
                    sh: Arc::clone(&sh),
                }
                .persist_budget(&who.username, &budget);
                let (ms, text) = {
                    let g = st.lock();
                    let live = g.live.as_ref();
                    (
                        live.and_then(|l| l.turn_started_at)
                            .map_or(0, |t| t.elapsed().as_millis() as u64),
                        turn_text(live.map(|l| l.live_text.as_str()).unwrap_or("")),
                    )
                };
                emit(AgentEvent::TurnFinished {
                    stop: stop.clone(),
                    cost_usd,
                    tokens,
                    ms,
                    text,
                });
                if verdict == BudgetVerdict::Exceeded {
                    let mut g = st.lock();
                    let from = g.session.status;
                    g.session
                        .transition(SessionStatus::Paused)
                        .map_err(|t| illegal_session(from, t))
                        .ok();
                    drop(g);
                    rt_persist_status(&sh, &st);
                    // Say it where it happened: this user's own surfaces.
                    emit(AgentEvent::Notice {
                        text: match budget.daily_usd_cap {
                            Some(cap) => format!(
                                "daily budget (${cap:.2}) reached — paused until tomorrow (override via CLI)"
                            ),
                            None => {
                                "daily token budget reached — paused until tomorrow".to_string()
                            }
                        },
                    });
                }
                if stop == "error" {
                    emit(AgentEvent::Notice {
                        text: "turn ended in error; see /status — nothing was re-submitted".into(),
                    });
                }
                // A cycle can emit several `turn_finished` events (auto-poke,
                // verify): the first settles the status, the later ones only
                // refresh the cumulative usage.
                // B15b: a scheduled cycle ends here — restore the full
                // compile before the idle boundary can drain another
                // queued message against the lean one.
                if st
                    .lock()
                    .live
                    .as_ref()
                    .is_some_and(|l| is_scheduled(l.current_turn_source.as_deref()))
                    && let Err(e) = (Runtime {
                        sh: Arc::clone(&sh),
                    })
                    .refresh_compiled_memory_for(&who.username)
                {
                    emit(AgentEvent::Error {
                        class: "internal".into(),
                        retryable: retryable_class("internal"),
                        message: format!("memory restore: {e}"),
                    });
                }
                rt_maybe_idle(&sh, &st);
                // Another user may be waiting for a worker slot.
                Runtime {
                    sh: Arc::clone(&sh),
                }
                .spawn_drain();
                invariants::check_and_report(&sh, &st);
            }
            PortEvent::Idle => {
                // Delivery boundary (SEED.md §3.7): flush the buffered final
                // answer through the outbox, then drain the user's queue.
                let (text, turn_id, sid, private) = {
                    let mut g = st.lock();
                    let mut text = String::new();
                    let mut turn_id = None;
                    if let Some(live) = g.live.as_mut() {
                        text = std::mem::take(&mut live.live_text);
                        turn_id = live.current_turn.clone();
                    }
                    let private = g.session.private;
                    (text, turn_id, g.session.id.clone(), private)
                };
                // `[SILENT]`: a scheduled turn with nothing to say delivers
                // nothing (the turn still finishes).
                let silent = text.trim_start().starts_with("[SILENT]");
                let scheduled = st
                    .lock()
                    .live
                    .as_ref()
                    .is_some_and(|l| is_scheduled(l.current_turn_source.as_deref()));
                if let Some(tid) = &turn_id
                    && !silent
                    && !text.trim().is_empty()
                {
                    if scheduled {
                        // Its streamed deltas were suppressed while it ran:
                        // deliver the whole answer once, here.
                        let mut whole = text.clone();
                        if !whole.ends_with('\n') {
                            whole.push('\n');
                        }
                        emit(AgentEvent::Delta { text: whole });
                    }
                    let parts = split_final_text(&text, FINAL_CHUNK_MAX);
                    for (i, chunk) in parts.iter().enumerate() {
                        // The outbox is the durable crash-recovery copy of
                        // the answer — a private conversation delivers
                        // through the FinalChunk events only.
                        if !private {
                            let mut oc = OutboxChunk::new(tid.clone(), i as u32, chunk.clone());
                            sh.store.push_chunk(&oc).ok();
                            oc.status = ka_agent_core::OutboxStatus::Sent;
                            oc.channel_message_id = Some(i as i64);
                            sh.store.mark_sent(&oc.turn, oc.chunk_index, i as i64).ok();
                        }
                        let index = if parts.len() > 1 { i as u32 + 1 } else { 1 };
                        emit(AgentEvent::FinalChunk {
                            index,
                            turn: tid.as_str().to_string(),
                            text: chunk.clone(),
                        });
                    }
                }
                // Episodic index (SEED.md §4.5): one user and one assistant
                // row per prompt cycle, at the delivery boundary. Private
                // conversations never enter it.
                if let (false, Some(tid)) = (private, &turn_id) {
                    let inbound = sh
                        .store
                        .get_turn(tid)
                        .ok()
                        .flatten()
                        .map(|t| t.inbound_text)
                        .unwrap_or_default();
                    sh.store.index_episode(&sid, tid, "user", &inbound).ok();
                    sh.store.index_episode(&sid, tid, "assistant", &text).ok();
                }
                // `idle` is a delivery boundary, not the end of the turn.
                let turn_open = {
                    let g = st.lock();
                    let status = g.session.status;
                    let current = g.live.as_ref().and_then(|l| l.current_turn.clone());
                    drop(g);
                    match current {
                        Some(tid) => sh
                            .store
                            .get_turn(&tid)
                            .ok()
                            .flatten()
                            .is_some_and(|t| !t.status.is_terminal()),
                        None => status == SessionStatus::Running,
                    }
                };
                // A pending ask owns the boundary too: the engine is blocked
                // until its decision or TTL arrives.
                let ask_pending = st
                    .lock()
                    .live
                    .as_ref()
                    .is_some_and(|l| !l.pending_asks.is_empty());
                // B15b: restore the full compile at the delivery boundary,
                // before any drain starts another turn.
                if scheduled
                    && let Err(e) = (Runtime {
                        sh: Arc::clone(&sh),
                    })
                    .refresh_compiled_memory_for(&who.username)
                {
                    emit(AgentEvent::Error {
                        class: "internal".into(),
                        retryable: retryable_class("internal"),
                        message: format!("memory restore: {e}"),
                    });
                }
                if !ask_pending && !turn_open {
                    // A cycle that finished done/length without one
                    // character of answer text (reasoning-only reply:
                    // empty content, everything in thinking) used to end
                    // in total silence — the turn was done, nothing was
                    // delivered and nobody was told. The turn record
                    // carries stop `empty` (the finish handler rewrites
                    // it). Ask the model once more (every fresh prompt
                    // earns one nudge; a nudge turn never nudges again —
                    // that is the loop bound), and when the re-ask comes
                    // back empty too, deliver an explicit message through
                    // the hardened chunk pipeline — a queued outbox row
                    // plus a FinalChunk — never a bare Notice, which a
                    // surface with no known chat drops on the floor.
                    if text.trim().is_empty()
                        && let Some(tid) = &turn_id
                        && sh
                            .store
                            .get_turn(tid)
                            .ok()
                            .flatten()
                            .is_some_and(|t| t.stop.as_deref() == Some("empty"))
                        // F12: an `executing` plan's empty answer is the
                        // verify cycle's problem (its repair queue owns
                        // the re-feed); nudging too would double-feed
                        // the same failure. A `drafting` plan keeps the
                        // nudge — its capture has no other retry.
                        && !nudge_suppressed_for_plan(&sh, &st)
                    {
                        let turn_from_nudge =
                            st.lock().live.as_ref().is_some_and(|l| l.turn_from_nudge);
                        if !turn_from_nudge {
                            let source = st
                                .lock()
                                .live
                                .as_ref()
                                .and_then(|l| l.current_turn_source.clone())
                                .unwrap_or_else(|| "console".into());
                            emit(AgentEvent::Note {
                                text: "empty reply from the model — asking again".into(),
                            });
                            let item = ka_agent_core::QueueItem::new(
                                sid.clone(),
                                format!(
                                    "{NUDGE_PREFIX} Your last turn ended with no answer \
                                     text — only reasoning. Send the actual answer now, as \
                                     plain message text."
                                ),
                                &source,
                            );
                            if let Err(e) = sh.store.enqueue(&item) {
                                emit(AgentEvent::Error {
                                    class: "internal".into(),
                                    retryable: retryable_class("internal"),
                                    message: format!("nudge enqueue: {e}"),
                                });
                            }
                        } else {
                            // The re-ask failed too. Quote the model's
                            // last notes when it left any — the reasoning
                            // often carries the conclusion the answer
                            // never did.
                            let notes = {
                                let g = st.lock();
                                g.live
                                    .as_ref()
                                    .map(|l| thought_excerpt(&l.thought_tail))
                                    .unwrap_or_default()
                            };
                            let mut give_up = "⚠️ the model returned an empty answer \
                                               twice (reasoning only, no text was \
                                               delivered)."
                                .to_string();
                            if !notes.is_empty() {
                                give_up.push_str(&format!("\n\nIts last notes: “{notes}”"));
                            }
                            give_up.push_str("\n\n/retry or rephrase.");
                            if !private {
                                let oc = OutboxChunk::new(tid.clone(), 0, give_up.clone());
                                sh.store.push_chunk(&oc).ok();
                                sh.store.mark_sent(tid, 0, 0).ok();
                            }
                            emit(AgentEvent::FinalChunk {
                                index: 1,
                                turn: tid.as_str().to_string(),
                                text: give_up,
                            });
                        }
                    }
                    // v4 F12 plan boundary: a `plan:<id>` turn either
                    // captures its answer into a drafting artifact or
                    // settles an executing one through the verify/repair
                    // cycle — before the drain, so a repair enqueued here
                    // starts in this same drain (single-writer untouched:
                    // the repair is an ordinary queued inbound).
                    plan_boundary(&sh, &who, &st, &text, turn_id.as_ref(), &sid).await;
                    drain_queue(&sh, &who, &st).await;
                }
                rt_maybe_idle(&sh, &st);
                invariants::check_and_report(&sh, &st);
            }
            PortEvent::Note(t) => emit(AgentEvent::Note { text: t }),
            PortEvent::EngineError {
                class,
                message,
                retryable,
            } => {
                // A retryable rate limit keeps retrying in-engine (the
                // port's backoff owns the loop): stamp the start so every
                // status surface shows the cooldown instead of a bare
                // error. The next settled turn clears it (TurnFinished).
                if class == "rate_limit" && retryable {
                    st.lock().throttled_since_ms = Some(ka_agent_core::now_ms());
                }
                emit(AgentEvent::Error {
                    class: class.clone(),
                    message: message.clone(),
                    retryable,
                });
                if class == "auth" {
                    let mut g = st.lock();
                    let from = g.session.status;
                    g.session
                        .transition(SessionStatus::Paused)
                        .map_err(|t| illegal_session(from, t))
                        .ok();
                    drop(g);
                    rt_persist_status(&sh, &st);
                    let remedy = cfg_of(&sh)
                        .model
                        .as_deref()
                        .and_then(|m| ka_agent_port::catalog::validate_selector(m).ok())
                        .and_then(|s| s.key_env);
                    let text = match remedy {
                        Some(env) => {
                            format!(
                                "the provider rejected the key — run `ka-agent key set {env}`, then /resume"
                            )
                        }
                        None => {
                            "auth failure — paused; fix the provider key and /resume".to_string()
                        }
                    };
                    emit(AgentEvent::Notice { text });
                }
            }
            PortEvent::EngineClosed => {
                let interrupted = { st.lock().live.as_ref().and_then(|l| l.current_turn.clone()) };
                if let Some(tid) = interrupted
                    && let Some(mut t) = sh.store.get_turn(&tid).ok().flatten()
                    && !t.status.is_terminal()
                {
                    t.transition(TurnStatus::Interrupted)
                        .map_err(|x| illegal_turn(t.status, x))
                        .ok();
                    sh.store.update_turn(&t).ok();
                }
                rt_on_engine_closed(&sh, &st, port.clone()).await;
                // B15b: the closed engine may have been mid-scheduled-turn
                // — put the full compile back (boot also self-heals).
                if st
                    .lock()
                    .live
                    .as_ref()
                    .is_some_and(|l| is_scheduled(l.current_turn_source.as_deref()))
                    && let Err(e) = (Runtime {
                        sh: Arc::clone(&sh),
                    })
                    .refresh_compiled_memory_for(&who.username)
                {
                    emit(AgentEvent::Error {
                        class: "internal".into(),
                        retryable: retryable_class("internal"),
                        message: format!("memory restore: {e}"),
                    });
                }
                invariants::check_and_report(&sh, &st);
            }
        }
    }
}

/// The engine ask id carried by an approval (only `KaAsk` cards have one).
fn approval_ask_id(a: &Approval) -> String {
    match &a.kind {
        ApprovalKind::KaAsk { ask_id, .. } => ask_id.clone(),
        ApprovalKind::Intent { .. } => String::new(),
    }
}

/// Outcome of one plan verify run: the zero-token gate's shape (exit
/// word) with the raw exit code for the audit row and the capped output
/// for the repair prompt.
#[derive(Debug)]
enum VerifyOutcome {
    /// Exit 0 (output discarded — nothing to repair).
    Pass { exit: i32 },
    /// Non-zero exit, or the gate could not run (spawn error, timeout).
    Fail {
        output: String,
        exit: Option<i32>,
        why: String,
    },
}

/// The `plan:<id>` source family (v4 F12): the plan id a turn belongs to.
fn plan_source(source: Option<&str>) -> Option<String> {
    source
        .and_then(|s| s.strip_prefix("plan:"))
        .map(str::trim)
        .filter(|id| !id.is_empty())
        .map(str::to_string)
}

/// The v4 F12 plan boundary, run inside `Idle` before the queue drain:
/// a `plan:<id>` turn either captures its answer into a `drafting`
/// artifact or settles an `executing` one through the verify/repair
/// cycle. Anything else (a plan another user owns, an unknown id) is a
/// no-op: the turn's answer still delivers as ordinary chat output.
async fn plan_boundary(
    sh: &Arc<Shared>,
    who: &Principal,
    st: &UserHandle,
    text: &str,
    turn_id: Option<&TurnId>,
    sid: &SessionId,
) {
    let source = st
        .lock()
        .live
        .as_ref()
        .and_then(|l| l.current_turn_source.clone());
    let Some(plan_id) = plan_source(source.as_deref()) else {
        return;
    };
    let Ok(Some(plan)) = sh.store.get_plan(&plan_id) else {
        return;
    };
    // The artifact belongs to its owner: a repair or capture never
    // lands on another user's plan.
    if plan.username != who.username {
        return;
    }
    match plan.status {
        PlanStatus::Drafting => plan_capture(sh, &plan, text, source.as_deref().unwrap_or("")),
        PlanStatus::Executing => {
            // The repair queues into the *live* session (where the
            // cycle's drain runs), not the plan's creation session.
            plan_verify_cycle(
                sh,
                who,
                st,
                &plan,
                turn_id,
                sid,
                source.as_deref().unwrap_or(""),
            )
            .await;
        }
        _ => {}
    }
}

/// Lift every `Verify:` line out of a captured plan body into the verify
/// column — the last one wins. `` `cmd` `` is unwrapped; `Verify: none`
/// (or a bare `Verify:`) records no gate.
fn split_verify_lines(body: &str) -> (String, Option<String>) {
    let mut verify: Option<String> = None;
    let mut kept: Vec<&str> = Vec::new();
    for line in body.lines() {
        let trimmed = line.trim();
        let rest = trimmed
            .strip_prefix("Verify:")
            .or_else(|| trimmed.strip_prefix("verify:"));
        match rest {
            Some(rest) => {
                let cmd = rest.trim().trim_matches('`').trim();
                verify =
                    (!cmd.is_empty() && !cmd.eq_ignore_ascii_case("none")).then(|| cmd.to_string());
            }
            None => kept.push(line),
        }
    }
    (kept.join("\n").trim().to_string(), verify)
}

/// Capture (v4 F12): the finished `plan:<id>` turn's whole cycle text
/// becomes the body of its `drafting` plan, the `Verify:` line lifts
/// into the column, and the row flips to `draft` (approval arms the
/// gate from here). An empty answer writes nothing — the empty-answer
/// nudge's re-ask (same source) appends instead; a still-empty cycle
/// stays `drafting` with the nudge's own notice.
fn plan_capture(sh: &Arc<Shared>, plan: &PlanRow, text: &str, source: &str) {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return;
    }
    let emit = |ev: AgentEvent| sh.emit_from(ev, Some(source));
    let combined = if plan.body.trim().is_empty() {
        trimmed.to_string()
    } else {
        format!("{}\n\n{trimmed}", plan.body.trim_end())
    };
    let (body, verify) = split_verify_lines(&combined);
    if let Err(e) = sh.store.set_plan_body(&plan.id, &body) {
        emit(AgentEvent::Error {
            class: "internal".into(),
            retryable: retryable_class("internal"),
            message: format!("plan capture: {e}"),
        });
        return;
    }
    if let Some(v) = verify.as_deref()
        && let Err(e) = sh.store.set_plan_verify(&plan.id, Some(v))
    {
        emit(AgentEvent::Error {
            class: "internal".into(),
            retryable: retryable_class("internal"),
            message: format!("plan capture: {e}"),
        });
        return;
    }
    match sh
        .store
        .transition_plan(&plan.id, PlanStatus::Drafting, PlanStatus::Draft)
    {
        Ok(()) => {
            let armed = verify
                .as_deref()
                .map(|v| format!("verify `{v}` unarmed until /plan approve"))
                .unwrap_or_else(|| "no verify line — runs gateless".into());
            emit(AgentEvent::Notice {
                text: format!(
                    "plan {} captured as a draft — {armed}",
                    ka_agent_core::ids::short_id(&plan.id)
                ),
            });
        }
        Err(e) => emit(AgentEvent::Error {
            class: "internal".into(),
            retryable: retryable_class("internal"),
            message: format!("plan capture: {e}"),
        }),
    }
}

/// Which finished turns owe a verify gate run: everything except the
/// engine-failure family — a turn ending `error`/`aborted`/`interrupted`
/// runs no gate and auto-submits nothing (SEED §7.3). `empty` is a
/// done-family stop (the model said nothing): the gate is still the
/// authority on done-ness.
fn plan_turn_gates(stop: Option<&str>) -> bool {
    !matches!(stop, Some("error") | Some("aborted") | Some("interrupted"))
}

/// Whether the empty-answer nudge must stand down for this cycle: an
/// `executing` plan's re-feed belongs to its repair queue (nudging too
/// would double-feed the same failure); a `drafting` plan keeps the
/// nudge — its capture has no other retry.
fn nudge_suppressed_for_plan(sh: &Arc<Shared>, st: &UserHandle) -> bool {
    st.lock()
        .live
        .as_ref()
        .and_then(|l| l.current_turn_source.as_deref())
        .and_then(|s| plan_source(Some(s)))
        .is_some_and(|id| {
            sh.store
                .get_plan(&id)
                .ok()
                .flatten()
                .is_some_and(|p| p.status == PlanStatus::Executing)
        })
}

/// Verify/repair (v4 F12): a gate-eligible turn of an `executing` plan
/// runs its verify gate — `sh -c`, the session workspace, 30 s ceiling,
/// capped output, `spawn_blocking`-wrapped so this async `drive` task
/// never blocks. Pass (or no verify at all) settles `done`; a failure
/// spends one repair iteration and enqueues exactly one follow-up turn
/// whose gate output rides the `<gate-output>` envelope; at the cap the
/// plan settles `failed`.
async fn plan_verify_cycle(
    sh: &Arc<Shared>,
    who: &Principal,
    st: &UserHandle,
    plan: &PlanRow,
    turn_id: Option<&TurnId>,
    sid: &SessionId,
    source: &str,
) {
    let emit = |ev: AgentEvent| sh.emit_from(ev, Some(source));
    let clean = turn_id
        .and_then(|tid| sh.store.get_turn(tid).ok().flatten())
        .is_some_and(|t| plan_turn_gates(t.stop.as_deref()));
    if !clean {
        return;
    }
    // Gateless approved plan: a clean turn is the whole check.
    let Some(gate) = plan.verify.clone() else {
        match sh
            .store
            .transition_plan(&plan.id, PlanStatus::Executing, PlanStatus::Done)
        {
            Ok(()) => emit(AgentEvent::Notice {
                text: format!(
                    "plan {} done — clean turn, no verify command",
                    ka_agent_core::ids::short_id(&plan.id)
                ),
            }),
            Err(e) => emit(AgentEvent::Error {
                class: "internal".into(),
                retryable: retryable_class("internal"),
                message: format!("plan settle: {e}"),
            }),
        }
        return;
    };
    let workspace = Runtime { sh: Arc::clone(sh) }.workspace_for(who);
    let run = move || run_verify_gate(&gate, &workspace);
    let outcome = match tokio::task::spawn_blocking(run).await {
        Ok(outcome) => outcome,
        Err(e) => VerifyOutcome::Fail {
            output: String::new(),
            exit: None,
            why: format!("gate task: {e}"),
        },
    };
    // Every gate execution lands an audit row — outcome and exit — a
    // local shell exactly like a cron gate (README's intents rule stays
    // scoped to external writes).
    let (word, outcome_detail) = match &outcome {
        VerifyOutcome::Pass { exit } => ("pass", format!("exit {exit}")),
        VerifyOutcome::Fail { exit, why, .. } => (
            "fail",
            match exit {
                Some(code) => format!("exit {code}: {why}"),
                None => why.clone(),
            },
        ),
    };
    let mut rec = AuditRecord::new(
        "runtime",
        audit_actions::PLAN_VERIFY,
        &plan.id,
        Some(format!(
            "{word}; repair {}/{}; {outcome_detail}",
            plan.iterations, plan.max_iterations
        )),
    )
    .with_user(&who.username)
    .with_role(&who.role);
    let _ = sh.store.append_audit(&mut rec);
    match outcome {
        VerifyOutcome::Pass { .. } => {
            match sh
                .store
                .transition_plan(&plan.id, PlanStatus::Executing, PlanStatus::Done)
            {
                Ok(()) => emit(AgentEvent::Notice {
                    text: format!(
                        "plan {} done — verify passed",
                        ka_agent_core::ids::short_id(&plan.id)
                    ),
                }),
                Err(e) => emit(AgentEvent::Error {
                    class: "internal".into(),
                    retryable: retryable_class("internal"),
                    message: format!("plan settle: {e}"),
                }),
            }
        }
        VerifyOutcome::Fail { output, why, .. } => {
            // The gate ran for up to 30 s: an inbound message may have
            // claimed the cycle in the meantime. Never race a live turn
            // (SEED §7.3 — the runtime does not auto-submit into a
            // running cycle); the plan stays `executing` and the
            // operator re-runs it.
            let boundary_taken = st.lock().live.as_ref().is_some_and(|l| {
                !l.pending_asks.is_empty()
                    || l.current_turn
                        .as_ref()
                        .and_then(|tid| sh.store.get_turn(tid).ok().flatten())
                        .is_some_and(|t| !t.status.is_terminal())
            });
            if boundary_taken {
                return;
            }
            // The cron budget check gates each repair: a window with
            // nothing left stays `executing` for the operator to re-run.
            if (Runtime { sh: Arc::clone(sh) }).budget_verdict_for(&who.username)
                == ka_agent_core::BudgetVerdict::Exceeded
            {
                emit(AgentEvent::Notice {
                    text: format!(
                        "plan {}: verify failed ({why}) — repair deferred, today's budget is reached",
                        ka_agent_core::ids::short_id(&plan.id)
                    ),
                });
                return;
            }
            match sh.store.bump_plan_iteration(&plan.id) {
                Ok(spent) => {
                    let shown = if output.trim().is_empty() {
                        why.clone()
                    } else {
                        output
                    };
                    let prompt = format!(
                        "The verify gate of plan {} failed (repair {}/{}). Make the \
                         workspace pass the gate, then summarize what you changed.\n\n\
                         <plan>\n{}\n</plan>\n\n<gate-output>\n{}\n</gate-output>",
                        plan.id,
                        spent,
                        plan.max_iterations,
                        plan.body,
                        shown.trim_end()
                    );
                    let item = ka_agent_core::QueueItem::new(sid.clone(), prompt, source);
                    match sh.store.enqueue(&item) {
                        Ok(()) => emit(AgentEvent::Notice {
                            text: format!(
                                "plan {}: verify failed ({why}) — repair {spent}/{} queued",
                                ka_agent_core::ids::short_id(&plan.id),
                                plan.max_iterations
                            ),
                        }),
                        Err(e) => emit(AgentEvent::Error {
                            class: "internal".into(),
                            retryable: retryable_class("internal"),
                            message: format!("plan repair enqueue: {e}"),
                        }),
                    }
                }
                // At the cap (or the plan settled underneath us).
                Err(e) => match sh.store.transition_plan(
                    &plan.id,
                    PlanStatus::Executing,
                    PlanStatus::Failed,
                ) {
                    Ok(()) => emit(AgentEvent::Notice {
                        text: format!(
                            "plan {} failed after {} repair(s) — verify: {why} ({e})",
                            ka_agent_core::ids::short_id(&plan.id),
                            plan.iterations
                        ),
                    }),
                    Err(settle) => emit(AgentEvent::Error {
                        class: "internal".into(),
                        retryable: retryable_class("internal"),
                        message: format!("plan settle: {settle}"),
                    }),
                },
            }
        }
    }
}

/// Run one plan verify gate: `sh -c <gate>` (Windows `cmd /C`) in the
/// session workspace, 30 s ceiling (kill), stdout+stderr capped at
/// 4 KiB — the `run_gate` shape, sync so it can sit in
/// `spawn_blocking`. Blocks the calling thread; never call from the
/// drive task directly.
fn run_verify_gate(gate: &str, workspace: &std::path::Path) -> VerifyOutcome {
    use std::io::Read as _;
    use std::process::{Command, Stdio};
    let mut cmd = if cfg!(windows) {
        let mut c = Command::new("cmd");
        c.args(["/C", gate]);
        c
    } else {
        let mut c = Command::new("sh");
        c.args(["-c", gate]);
        c
    };
    // A workspace that does not exist yet (a fresh instance, a test
    // harness) falls back to the process cwd rather than failing the
    // gate with ENOENT — the same fallback `run_gate` uses.
    let cwd = if workspace.is_dir() {
        workspace.to_path_buf()
    } else {
        std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("/"))
    };
    cmd.current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            return VerifyOutcome::Fail {
                output: String::new(),
                exit: None,
                why: format!("spawn: {e}"),
            };
        }
    };
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let mut buf = Vec::new();
                if let Some(mut stdout) = child.stdout.take() {
                    let _ = stdout.read_to_end(&mut buf);
                }
                if let Some(mut stderr) = child.stderr.take() {
                    let _ = stderr.read_to_end(&mut buf);
                }
                let output = String::from_utf8_lossy(&buf);
                let output: String = output.chars().take(4096).collect();
                let code = status.code();
                return if status.success() {
                    VerifyOutcome::Pass {
                        exit: code.unwrap_or(0),
                    }
                } else {
                    VerifyOutcome::Fail {
                        output,
                        exit: code,
                        why: format!(
                            "gate `{gate}` exited {}",
                            code.map(|c| c.to_string())
                                .unwrap_or_else(|| "by signal".into())
                        ),
                    }
                };
            }
            Ok(None) => {
                if std::time::Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return VerifyOutcome::Fail {
                        output: String::new(),
                        exit: None,
                        why: "timed out after 30s".into(),
                    };
                }
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            Err(e) => {
                return VerifyOutcome::Fail {
                    output: String::new(),
                    exit: None,
                    why: format!("wait: {e}"),
                };
            }
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::TurnAnnouncements;

    #[test]
    fn verify_line_lifts_out_of_the_body() {
        let body = "# Plan\n1. step\nVerify: `cargo test -q`\n2. more";
        let (body, verify) = super::split_verify_lines(body);
        assert_eq!(body, "# Plan\n1. step\n2. more");
        assert_eq!(verify.as_deref(), Some("cargo test -q"));
        // `none` records no gate; last one wins.
        let (body, verify) = super::split_verify_lines("Verify: a\nb\nverify: none");
        assert_eq!(body, "b");
        assert_eq!(verify, None);
        let (_, verify) = super::split_verify_lines("a\nverify: none\nVerify: `true`");
        assert_eq!(verify.as_deref(), Some("true"));
        // No line, no gate — and the body passes through untouched.
        let (body, verify) = super::split_verify_lines("just steps");
        assert_eq!(body, "just steps");
        assert_eq!(verify, None);
    }

    #[test]
    fn plan_source_parses_the_family() {
        assert_eq!(
            super::plan_source(Some("plan:plan_ab-12")),
            Some("plan_ab-12".to_string())
        );
        assert_eq!(super::plan_source(Some("cron")), None);
        assert_eq!(super::plan_source(Some("plan:")), None);
        assert_eq!(super::plan_source(None), None);
    }

    #[test]
    fn gate_runs_on_everything_but_engine_failures() {
        assert!(super::plan_turn_gates(Some("done")));
        assert!(super::plan_turn_gates(Some("length")));
        assert!(super::plan_turn_gates(Some("empty")));
        assert!(!super::plan_turn_gates(Some("error")));
        assert!(!super::plan_turn_gates(Some("aborted")));
        assert!(!super::plan_turn_gates(Some("interrupted")));
        assert!(super::plan_turn_gates(None));
    }

    #[test]
    fn verify_gate_runs_and_reports_exits() {
        let ws = std::env::temp_dir();
        match super::run_verify_gate("echo ok", &ws) {
            super::VerifyOutcome::Pass { exit } => {
                assert_eq!(exit, 0);
            }
            other => panic!("expected pass, got {other:?}"),
        }
        match super::run_verify_gate("echo bad >&2; exit 3", &ws) {
            super::VerifyOutcome::Fail { output, exit, .. } => {
                assert_eq!(exit, Some(3));
                assert!(output.contains("bad"), "{output}");
            }
            other => panic!("expected fail, got {other:?}"),
        }
    }

    #[test]
    fn one_announcement_per_runtime_turn() {
        let mut announcements = TurnAnnouncements::default();
        assert!(announcements.admit("t1"));
        assert!(
            !announcements.admit("t1"),
            "a repeated start inside the same submit→idle cycle (auto-poke, verify) is not a new turn"
        );
        assert!(
            announcements.admit("t2"),
            "the next runtime turn carries a new recorded id"
        );
        assert!(!announcements.admit("t2"));
    }

    #[test]
    fn an_unclaimed_start_is_announced_only_once() {
        let mut announcements = TurnAnnouncements::default();
        assert!(
            announcements.admit(""),
            "no recorded turn yet: the surface still gets one reset"
        );
        assert!(
            !announcements.admit(""),
            "a second start without a recorded turn must not reset again"
        );
        assert!(announcements.admit("t1"));
    }
}
