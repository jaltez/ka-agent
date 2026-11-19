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

/// Final-answer chunk ceiling before splitting (Telegram 4096 minus margin;
/// SEED.md §5.4).
pub const FINAL_CHUNK_MAX: usize = 3800;

/// Is this turn one a schedule started (cron / briefing)?
pub(crate) fn is_scheduled(source: Option<&str>) -> bool {
    source.is_some_and(|s| s == "cron" || s.starts_with("cron:") || s == "briefing")
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
    let collapsed: String = tail
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
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
                    g.live.as_ref().and_then(|live| live.tool_excerpt.get(&id)).cloned()
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
            PortEvent::Compacting { done } => emit(AgentEvent::Compacting { done }),
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
                let spec = (who.role != ka_agent_core::ADMIN_ROLE)
                    .then(|| Runtime { sh: Arc::clone(&sh) }.effective_role(&who.role));
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
                        .answer(approval_ask_id(&approval), ApprovalDecision::Deny.ka_choice())
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
                let empty_final = matches!(stop.as_str(), "done" | "length")
                    && full_text.trim().is_empty();
                if let Some(tid) = turn_result.clone()
                    && let Some(mut t) = sh.store.get_turn(&tid).ok().flatten()
                {
                    t.stop = Some(if empty_final { "empty".into() } else { stop.clone() });
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
                        t.transition(next).map_err(|x| illegal_turn(t.status, x)).ok();
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
                        for (i, chunk) in
                            split_final_text(&full_text, FINAL_CHUNK_MAX).iter().enumerate()
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
                Runtime { sh: Arc::clone(&sh) }.persist_budget(&who.username, &budget);
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
                            None => "daily token budget reached — paused until tomorrow".to_string(),
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
                rt_maybe_idle(&sh, &st);
                // Another user may be waiting for a worker slot.
                Runtime { sh: Arc::clone(&sh) }.spawn_drain();
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
                    {
                        let turn_from_nudge = st
                            .lock()
                            .live
                            .as_ref()
                            .is_some_and(|l| l.turn_from_nudge);
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
                                let oc =
                                    OutboxChunk::new(tid.clone(), 0, give_up.clone());
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

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::TurnAnnouncements;

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
