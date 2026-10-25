// Event pump: port events -> surface + store, turn lifecycle, final-answer
// chunking. Split from runtime.rs (moves only).
use super::approvals::session_allow_key;
use super::intake::drain_queue;
use super::worker::rt_on_engine_closed;
use super::*;

/// Final-answer chunk ceiling before splitting (Telegram 4096 minus margin;
/// SEED.md §5.4).
pub const FINAL_CHUNK_MAX: usize = 3800;

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

/// Return the session to `idle` once nothing is left in flight: the turn's
/// record is terminal (or no turn is recorded and no prompt is being handed
/// over), no ask is pending and the queue is empty. Called from both the
/// `turn_finished` and `idle` handlers — the turn record going terminal is
/// what proves the cycle is over, so the engine's `idle` event is only the
/// delivery boundary and its timing can never stall the status.
pub(super) fn rt_maybe_idle(sh: &Arc<Shared>) {
    let (sid, status, ask_pending, current) = {
        let g = sh.inner.lock().unwrap_or_else(|e| e.into_inner());
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
    let mut g = sh.inner.lock().unwrap_or_else(|e| e.into_inner());
    let from = g.session.status;
    g.session
        .transition(SessionStatus::Idle)
        .map_err(|t| illegal_session(from, t))
        .ok();
    let status = g.session.status;
    drop(g);
    rt_persist_status(sh, status);
    sh.surface.on_session_status(status);
}

pub(super) fn rt_persist_status(sh: &Arc<Shared>, status: SessionStatus) {
    let sid = sh
        .inner
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .session
        .id
        .clone();
    if let Err(e) = sh.store.update_session_status(&sid, status) {
        sh.surface
            .on_error("internal", &format!("session status persist: {e}"));
    }
}

/// The per-worker event loop: maps port events onto state, store and surface
/// (SEED.md §3.7: only events after our own submission are live output; the
/// delivery boundary is `idle`).
pub(super) async fn drive(
    sh: Arc<Shared>,
    port: Arc<PortSession>,
    mut rx: mpsc::Receiver<PortEvent>,
) {
    while let Some(ev) = rx.recv().await {
        // Any event is activity: the idle reap's clock resets.
        {
            let mut g = sh.inner.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(live) = g.live.as_mut() {
                live.last_activity_ms = ka_agent_core::now_ms();
            }
        }
        match ev {
            PortEvent::Bootstrap {
                strand_id,
                strand_path,
                replay_messages,
            } => {
                let mut g = sh.inner.lock().unwrap_or_else(|e| e.into_inner());
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
                    sh.surface
                        .on_error("internal", &format!("strand persist: {e}"));
                }
                if replay_messages > 0 {
                    sh.surface.on_note(&format!(
                        "resumed session: {replay_messages} replayed messages (not re-delivered)"
                    ));
                }
            }
            PortEvent::TurnStarted => sh.surface.on_turn_started(),
            PortEvent::DeltaText(t) => {
                let mut g = sh.inner.lock().unwrap_or_else(|e| e.into_inner());
                if let Some(live) = g.live.as_mut() {
                    live.live_text.push_str(&t);
                }
                drop(g);
                sh.surface.on_delta_text(&t);
            }
            PortEvent::DeltaThought(t) => sh.surface.on_delta_thought(&t),
            PortEvent::CallStarted { tool, detail, .. } => {
                sh.surface.on_note(format!("▸ {tool} {detail}").trim_end());
                let mut g = sh.inner.lock().unwrap_or_else(|e| e.into_inner());
                if let Some(live) = g.live.as_mut() {
                    live.last_call = Some((tool, detail));
                }
            }
            PortEvent::CallFinished {
                tool, ok: false, ..
            } => {
                sh.surface.on_note(&format!("✗ {tool} failed"));
            }
            PortEvent::CallFinished { ok: true, .. } => {}
            PortEvent::CallOutput {
                is_error: true,
                excerpt,
                ..
            } => {
                let first = excerpt.lines().next().unwrap_or("");
                sh.surface.on_note(&format!("  error: {first}"));
            }
            PortEvent::CallOutput {
                is_error: false, ..
            } => {}
            PortEvent::Ask {
                ask_id,
                question,
                detail,
                options,
            } => {
                let (sid, turn_id, ttl, last_call) = {
                    let g = sh.inner.lock().unwrap_or_else(|e| e.into_inner());
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
                let key = session_allow_key(&question, detail.as_deref());
                let pre_allowed = {
                    let g = sh.inner.lock().unwrap_or_else(|e| e.into_inner());
                    g.session_allow.contains(&key)
                };
                if pre_allowed {
                    let port = {
                        let g = sh.inner.lock().unwrap_or_else(|e| e.into_inner());
                        g.live.as_ref().map(|l| Arc::clone(&l.port))
                    };
                    if let Some(port) = port {
                        let _ = port
                            .answer(ask_id.clone(), ApprovalDecision::Allow.ka_choice())
                            .await;
                    }
                    let mut rec = AuditRecord::new(
                        "operator",
                        audit_actions::APPROVAL_DECIDE,
                        &ask_id,
                        Some("Allow (session)".into()),
                    );
                    let _ = sh.store.append_audit(&mut rec);
                    sh.surface
                        .on_note(&format!("✓ allowed for this session: {question}"));
                    continue;
                }
                let kind = ApprovalKind::KaAsk {
                    ask_id: ask_id.clone(),
                    question: question.clone(),
                    detail: detail.clone(),
                    options: options.clone(),
                };
                let approval = Approval::new(sid, turn_id, kind, ttl as i64);
                let card = ApprovalCard::from_approval(&approval);
                if let Err(e) = sh.store.insert_approval(&approval) {
                    sh.surface
                        .on_error("internal", &format!("approval persist: {e}"));
                }
                // An ask is only actionable while its turn is live. The engine
                // can raise one after it already reported `idle` (the scripted
                // speaker finishes its response before the permission gate, and
                // a real engine can lose the same race), and no surface can
                // settle an ask for a turn that is over. Deny it (fail closed)
                // so the engine is never left blocked on its gate.
                let (awaiting, live_port, status) = {
                    let mut g = sh.inner.lock().unwrap_or_else(|e| e.into_inner());
                    let from = g.session.status;
                    let awaiting = g
                        .session
                        .transition(SessionStatus::AwaitingApproval)
                        .map_err(|t| illegal_session(from, t))
                        .is_ok();
                    if awaiting && let Some(live) = g.live.as_mut() {
                        live.pending_asks
                            .insert(approval.id.clone(), ask_id.clone());
                    }
                    let port = g.live.as_ref().map(|l| Arc::clone(&l.port));
                    let status = g.session.status;
                    drop(g);
                    (awaiting, port, status)
                };
                if awaiting {
                    rt_persist_status(&sh, status);
                    if let Some(card) = card {
                        sh.surface.on_approval(&card);
                    }
                    continue;
                }
                if let Some(port) = live_port {
                    let _ = port
                        .answer(ask_id.clone(), ApprovalDecision::Deny.ka_choice())
                        .await;
                }
                if let Err(e) = sh.store.decide_approval(
                    &approval.id,
                    ApprovalStatus::Denied,
                    ka_agent_core::now_ms(),
                ) {
                    sh.surface
                        .on_error("internal", &format!("ask deny persist: {e}"));
                }
                let mut rec = AuditRecord::new(
                    "runtime",
                    audit_actions::APPROVAL_DECIDE,
                    approval.id.as_str(),
                    Some("deny: the turn this ask belongs to already finished".into()),
                );
                let _ = sh.store.append_audit(&mut rec);
                sh.surface
                    .on_notice("ask auto-denied: its turn already finished (nothing to approve)");
            }
            PortEvent::TurnFinished {
                stop,
                cost_usd,
                tokens,
            } => {
                let turn_result = {
                    let g = sh.inner.lock().unwrap_or_else(|e| e.into_inner());
                    g.live.as_ref().and_then(|l| l.current_turn.clone())
                };
                if let Some(tid) = turn_result
                    && let Some(mut t) = sh.store.get_turn(&tid).ok().flatten()
                {
                    t.stop = Some(stop.clone());
                    t.cost_usd = cost_usd;
                    t.tokens = tokens;
                    // The engine can finish a turn before the submit
                    // handshake recorded it as running: walk the record
                    // up so the finish always lands on a legal edge.
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
                        sh.surface
                            .on_error("internal", &format!("turn persist: {e}"));
                    }
                }
                let (verdict, budget) = {
                    let mut g = sh.inner.lock().unwrap_or_else(|e| e.into_inner());
                    let now = ka_agent_core::now_ms();
                    g.budget.record(cost_usd, tokens, now);
                    let b = g.budget.clone();
                    (
                        g.budget.verdict(),
                        serde_json::to_string(&b).unwrap_or_default(),
                    )
                };
                sh.store.meta_set("budget", &budget).ok();
                sh.surface.on_turn_finished(&stop, cost_usd, tokens);
                if verdict == BudgetVerdict::Exceeded {
                    let mut g = sh.inner.lock().unwrap_or_else(|e| e.into_inner());
                    let from = g.session.status;
                    g.session
                        .transition(SessionStatus::Paused)
                        .map_err(|t| illegal_session(from, t))
                        .ok();
                    let status = g.session.status;
                    drop(g);
                    rt_persist_status(&sh, status);
                    sh.surface.on_notice(
                        "daily budget reached — paused until tomorrow (override via CLI)",
                    );
                }
                if stop == "error" {
                    sh.surface
                        .on_notice("turn ended in error; see /status — nothing was re-submitted");
                }
                // A cycle can emit several `turn_finished` events (auto-poke,
                // verify): the first settles the status, the later ones only
                // refresh the cumulative usage.
                rt_maybe_idle(&sh);
            }
            PortEvent::Idle => {
                // Delivery boundary (SEED.md §3.7): flush the buffered final
                // answer through the outbox, then drain the durable queue.
                let (text, turn_id, sid) = {
                    let mut g = sh.inner.lock().unwrap_or_else(|e| e.into_inner());
                    let mut text = String::new();
                    let mut turn_id = None;
                    if let Some(live) = g.live.as_mut() {
                        text = std::mem::take(&mut live.live_text);
                        turn_id = live.current_turn.clone();
                    }
                    (text, turn_id, g.session.id.clone())
                };
                if let Some(tid) = &turn_id {
                    let parts = split_final_text(&text, FINAL_CHUNK_MAX);
                    for (i, chunk) in parts.iter().enumerate() {
                        let mut oc = OutboxChunk::new(tid.clone(), i as u32, chunk.clone());
                        sh.store.push_chunk(&oc).ok();
                        if parts.len() > 1 {
                            sh.surface.on_final_chunk(i as u32 + 1, chunk);
                        } else {
                            sh.surface.on_final_chunk(1, chunk);
                        }
                        oc.status = ka_agent_core::OutboxStatus::Sent;
                        oc.channel_message_id = Some(i as i64);
                        sh.store.mark_sent(&oc.turn, oc.chunk_index, i as i64).ok();
                    }
                }
                // Episodic index (SEED.md §4.5): one user and one assistant
                // row per prompt cycle, at the delivery boundary.
                if let Some(tid) = &turn_id {
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
                // `idle` is a delivery boundary, not the end of the turn. ka
                // reports it for the cycle — including while a submitted turn
                // is still open (engine start-up, a permission gate about to
                // open) — and acting on it as if the turn were over orphans
                // the turn: later messages would be submitted on top of it
                // instead of queuing, an ask would arrive for a session that
                // no longer looks busy, and draining the queue here would
                // hand ka a prompt it drops (`PortError::Busy`, gap G4). The
                // turn's own record decides.
                let turn_open = {
                    let g = sh.inner.lock().unwrap_or_else(|e| e.into_inner());
                    let status = g.session.status;
                    let current = g.live.as_ref().and_then(|l| l.current_turn.clone());
                    drop(g);
                    match current {
                        // A turn is in flight until its own record says it is over.
                        Some(tid) => sh
                            .store
                            .get_turn(&tid)
                            .ok()
                            .flatten()
                            .is_some_and(|t| !t.status.is_terminal()),
                        // No turn recorded yet, but an inbound message moved the
                        // session to `running`: its prompt is still being handed
                        // over, and the engine's start-up `idle` must not end it
                        // (that would make the next message overtake the prompt
                        // ka is about to accept).
                        None => status == SessionStatus::Running,
                    }
                };
                // A pending ask owns the boundary too: the engine is blocked
                // until its decision or TTL arrives.
                let ask_pending = {
                    let g = sh.inner.lock().unwrap_or_else(|e| e.into_inner());
                    g.live.as_ref().is_some_and(|l| !l.pending_asks.is_empty())
                };
                if !ask_pending && !turn_open {
                    drain_queue(&sh).await;
                }
                rt_maybe_idle(&sh);
            }
            PortEvent::Note(t) => sh.surface.on_note(&t),
            PortEvent::EngineError { class, message, .. } => {
                sh.surface.on_error(&class, &message);
                if class == "auth" {
                    let mut g = sh.inner.lock().unwrap_or_else(|e| e.into_inner());
                    let from = g.session.status;
                    g.session
                        .transition(SessionStatus::Paused)
                        .map_err(|t| illegal_session(from, t))
                        .ok();
                    let status = g.session.status;
                    drop(g);
                    rt_persist_status(&sh, status);
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
                    sh.surface.on_notice(&text);
                }
            }
            PortEvent::EngineClosed => {
                let interrupted = {
                    let g = sh.inner.lock().unwrap_or_else(|e| e.into_inner());
                    g.live.as_ref().and_then(|l| l.current_turn.clone())
                };
                if let Some(tid) = interrupted
                    && let Some(mut t) = sh.store.get_turn(&tid).ok().flatten()
                    && !t.status.is_terminal()
                {
                    t.transition(TurnStatus::Interrupted)
                        .map_err(|x| illegal_turn(t.status, x))
                        .ok();
                    sh.store.update_turn(&t).ok();
                }
                rt_on_engine_closed(&sh, port.clone()).await;
            }
        }
    }
}
