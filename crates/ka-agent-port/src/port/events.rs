//! The port event projection and the ka-event mapping. Split from
//! port.rs (moves only).

use serde::{Deserialize, Serialize};

/// The port's projection of engine activity — ka events reduced to what
/// Ka Agent consumes (docs/contract/ka-port.md §3).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum PortEvent {
    /// The engine attached: replayed history length plus the strand
    /// identity (id always, path once the strand file exists on disk —
    /// ka materializes a fresh strand only on first append).
    Bootstrap {
        /// Strand id, once known.
        strand_id: Option<String>,
        /// Strand file path, once known.
        strand_path: Option<String>,
        /// Messages in the replay.
        replay_messages: usize,
    },
    /// A turn began.
    TurnStarted,
    /// Visible assistant text delta.
    DeltaText(String),
    /// Reasoning/thought delta.
    DeltaThought(String),
    /// A tool call began.
    CallStarted {
        /// Tool name.
        tool: String,
        /// Call id.
        id: String,
        /// Argument summary.
        detail: String,
    },
    /// A tool call finished.
    CallFinished {
        /// Tool name.
        tool: String,
        /// Call id.
        id: String,
        /// Whether the call succeeded.
        ok: bool,
    },
    /// A tool call produced output.
    CallOutput {
        /// Tool name.
        tool: String,
        /// Call id.
        id: String,
        /// Capped excerpt.
        excerpt: String,
        /// Whether the output is an error result.
        is_error: bool,
    },
    /// The engine asks the operator to resolve a permission question.
    Ask {
        /// Ask id (used in `answer`).
        ask_id: String,
        /// First question text.
        question: String,
        /// Rendered detail (e.g. a diff), when present.
        detail: Option<String>,
        /// The first question's options, verbatim (index-referenced in
        /// `answer`).
        options: Vec<String>,
    },
    /// A turn finished.
    TurnFinished {
        /// `done` | `length` | `aborted` | `error`.
        stop: String,
        /// USD cost of the turn.
        cost_usd: f64,
        /// input + output + cache_read + cache_write tokens.
        tokens: u64,
    },
    /// The prompt cycle settled; the engine is idle. Delivery boundary
    /// for one turn (SEED §3.7).
    Idle,
    /// Informational engine note.
    Note(String),
    /// Engine-level error report.
    EngineError {
        /// `auth` | `rate_limit` | `overflow` | `network` | `protocol` |
        /// `unsupported` | `internal` (or `fatal` for worker-level
        /// failures reported through `WorkerEvent::Fatal`).
        class: String,
        /// Whether a retry could help.
        retryable: bool,
        /// Human-readable detail.
        message: String,
    },
    /// The engine (or worker) is gone; the session is dead.
    EngineClosed,
    /// The model's live todo list (`todos`): whole-list replacement of
    /// `(text, state)` pairs, in display order.
    Plan(Vec<(String, String)>),
    /// Live context meter during a turn.
    Context { used: u64, window: u64 },
    /// The strand's display title (empty titles are dropped by the
    /// runtime, not the port).
    Title(String),
    /// A context digest (compaction) started (`false`) or finished.
    Compacting { done: bool },
    /// Bootstrap inventory: the tool names this engine offers.
    Inventory { tools: Vec<String> },
    /// The engine reported a different model than the configured one (a
    /// fallback hop, or a live selector change): surfaces show it as the
    /// model actually answering (F3).
    ModelChanged(String),
    /// The context-usage breakdown answering `context_breakdown`
    /// (`/context`): estimated tokens per component, in engine order,
    /// plus the active window (0 = unknown). The engine appends
    /// `hint:`-prefixed trim-opportunity rows: they are advice, not
    /// usage, and surfaces leave them out of used/free totals.
    ContextBreakdown {
        /// `(component name, estimated tokens)`.
        parts: Vec<(String, u64)>,
        /// Context window of the active model (0 = unknown).
        window: u64,
    },
    /// The fast role's answer to `side_question` (`/btw`). An engine
    /// with no `[roles] fast` answers with a plain `Note`
    /// (`side question unavailable`).
    SideAnswer {
        /// The answer text.
        text: String,
    },
    /// The engine's answer to `session_diff` (`/diff`): the summary
    /// line (`no changes`, or `N file(s), X +/- lines — full:
    /// spill://<id>`), a blank line, then the 8k-capped excerpt. The
    /// full diff is the spill file the pointer names
    /// ([`crate::spill_path`]).
    SessionDiff {
        /// The summary line plus the excerpt.
        summary: String,
    },
    /// An idle recap: what the fast role made of the session after
    /// `[context] idle_minutes` of quiet (`recap.idle_minutes`). The
    /// engine also records it on the strand (`x.ka.recap`). An engine
    /// with no `[roles] fast` notes `idle recap unavailable: …` instead,
    /// which stays a plain `Note`.
    Recap {
        /// The recap text.
        text: String,
    },
}

/// Map one ka event to its port projection; `None` drops it. Dropped by
/// design: `session_info` (captured as strand identity), the bootstrap
/// state echo (`model_changed`, `mode_changed`, `effort_changed`,
/// `features_changed`), call-kind deltas, and every surface-specific
/// event. Bootstrap state is verified by contract tests, not streamed
/// upward. `todos`, `context_meter`, `title`, `digest_*`, `inventory`
/// and `context_breakdown` project to their port forms (plan, context,
/// title, compacting, inventory, context breakdown); the answers ka
/// carries on `Event::Note` are told apart in [`project_note`]; the
/// runtime decides what is broadcast.
pub(crate) fn map_ka_event(
    ev: ka_protocol::Event,
    strand_id: Option<&str>,
    strand_path: Option<&str>,
) -> Option<PortEvent> {
    use ka_protocol::DeltaKind;
    use ka_protocol::Event as E;

    Some(match ev {
        E::Replay { messages } => PortEvent::Bootstrap {
            strand_id: strand_id.map(str::to_string),
            strand_path: strand_path.map(str::to_string),
            replay_messages: messages.len(),
        },
        E::TurnStarted { .. } => PortEvent::TurnStarted,
        E::ContextMeter { used, window } => PortEvent::Context { used, window },
        E::Title { title } => PortEvent::Title(title),
        E::DigestStarted => PortEvent::Compacting { done: false },
        E::DigestFinished { .. } => PortEvent::Compacting { done: true },
        E::Inventory { tools, .. } => PortEvent::Inventory { tools },
        E::Todos { items } => PortEvent::Plan(
            items
                .into_iter()
                .map(|t| {
                    let state = match t.state {
                        ka_protocol::TodoState::Pending => "pending",
                        ka_protocol::TodoState::Done => "done",
                    };
                    (t.text, state.to_string())
                })
                .collect(),
        ),
        E::Delta {
            kind: DeltaKind::Text(text),
        } => PortEvent::DeltaText(text),
        E::Delta {
            kind: DeltaKind::Thought(text),
        } => PortEvent::DeltaThought(text),
        E::Delta {
            kind: DeltaKind::Call { .. },
        } => return None,
        E::CallStarted { tool, id, detail } => PortEvent::CallStarted { tool, id, detail },
        E::CallFinished { tool, id, ok } => PortEvent::CallFinished { tool, id, ok },
        E::CallOutput {
            tool,
            id,
            excerpt,
            is_error,
            ..
        } => PortEvent::CallOutput {
            tool,
            id,
            excerpt,
            is_error,
        },
        E::Ask { id, questions } => {
            // first question only, but keep all its options
            let q = questions.first();
            PortEvent::Ask {
                ask_id: id.0,
                question: q.map(|q| q.text.clone()).unwrap_or_default(),
                detail: q.and_then(|q| q.detail.clone()),
                options: q.map(|q| q.options.clone()).unwrap_or_default(),
            }
        }
        E::TurnFinished { stop, usage } => PortEvent::TurnFinished {
            stop: stop_word(stop),
            cost_usd: usage.cost,
            tokens: usage.input + usage.output + usage.cache_read + usage.cache_write,
        },
        E::Idle => PortEvent::Idle,
        E::ContextBreakdown { parts, window } => PortEvent::ContextBreakdown {
            parts: parts
                .into_iter()
                .map(|part| (part.name, part.tokens))
                .collect(),
            window,
        },
        E::Note { message } => project_note(message),
        // A fallback hop or a live selector change must be visible: the
        // operator has to know which model actually answered (F3).
        E::ModelChanged { selector } => PortEvent::ModelChanged(selector),
        E::Error {
            class,
            retryable,
            message,
        } => PortEvent::EngineError {
            class: error_class_word(class),
            retryable,
            message,
        },
        _ => return None,
    })
}

/// The prefixes ka puts on the notes that answer `side_question`
/// (`Command::SideQuestion` → `btw: <answer>`), `session_diff`
/// (`session diff: <summary>`), and on its idle recap (`idle recap:
/// <recap>`) — ka `crates/ka-engine/src/engine.rs`. The engine has no
/// event type for any of the three: each rides `Event::Note`, so the
/// prefix is the one thing that sets them apart from every other note,
/// and this is the only place that reads it. The engine's degrade notes
/// (`side question unavailable`, `idle recap unavailable: …`) carry no
/// answer and stay plain notes. Residual race, inherent to the wire: a
/// note whose text something inside the engine controls (a `.ka/hooks`
/// failure reason reaches `Event::Note` unprefixed) and that begins with
/// one of these markers projects as that answer.
const SIDE_ANSWER_PREFIX: &str = "btw: ";
const SESSION_DIFF_PREFIX: &str = "session diff: ";
const RECAP_PREFIX: &str = "idle recap: ";

/// Project one ka note by its marker prefix (see the constants above).
fn project_note(message: String) -> PortEvent {
    if let Some(text) = message.strip_prefix(SIDE_ANSWER_PREFIX) {
        PortEvent::SideAnswer {
            text: text.to_string(),
        }
    } else if let Some(summary) = message.strip_prefix(SESSION_DIFF_PREFIX) {
        PortEvent::SessionDiff {
            summary: summary.to_string(),
        }
    } else if let Some(text) = message.strip_prefix(RECAP_PREFIX) {
        PortEvent::Recap {
            text: text.to_string(),
        }
    } else {
        PortEvent::Note(message)
    }
}

/// The marker of the engine's reply to a `side_question` it could not
/// take — no `[roles] fast`, a failed or empty call. It carries no
/// answer, so [`project_note`] leaves it a plain `Note`; but it is what
/// the engine says to the command, and a runtime with a `/btw` in
/// flight recognizes it by this prefix. Named here so the engine's
/// markers stay in one crate.
pub const SIDE_ANSWER_UNAVAILABLE: &str = "side question unavailable";

fn stop_word(stop: ka_protocol::Stop) -> String {
    match stop {
        ka_protocol::Stop::Done => "done",
        ka_protocol::Stop::Length => "length",
        ka_protocol::Stop::Aborted => "aborted",
        ka_protocol::Stop::Error => "error",
    }
    .to_string()
}

fn error_class_word(class: ka_protocol::ErrorClass) -> String {
    match class {
        ka_protocol::ErrorClass::Auth => "auth",
        ka_protocol::ErrorClass::RateLimit => "rate_limit",
        ka_protocol::ErrorClass::Overflow => "overflow",
        ka_protocol::ErrorClass::Network => "network",
        ka_protocol::ErrorClass::Protocol => "protocol",
        ka_protocol::ErrorClass::Unsupported => "unsupported",
        ka_protocol::ErrorClass::Internal => "internal",
    }
    .to_string()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;
    use ka_protocol::{AskQuestion, DeltaKind, Event, Stop, Usage};

    #[test]
    fn replay_maps_to_bootstrap_with_strand_identity() {
        let ev = Event::Replay {
            messages: vec![
                ka_protocol::ReplayedMessage {
                    role: "user".into(),
                    content: "a".into(),
                    digest: false,
                    thinking: None,
                    calls: Vec::new(),
                };
                3
            ],
        };
        assert_eq!(
            map_ka_event(ev, Some("s1-x"), Some("/strands/s1-x.jsonl")),
            Some(PortEvent::Bootstrap {
                strand_id: Some("s1-x".into()),
                strand_path: Some("/strands/s1-x.jsonl".into()),
                replay_messages: 3,
            })
        );
    }

    #[test]
    fn deltas_and_calls_map_one_to_one() {
        assert_eq!(
            map_ka_event(
                Event::Delta {
                    kind: DeltaKind::Text("hi".into()),
                },
                None,
                None
            ),
            Some(PortEvent::DeltaText("hi".into()))
        );
        assert_eq!(
            map_ka_event(
                Event::Delta {
                    kind: DeltaKind::Thought("hm".into()),
                },
                None,
                None
            ),
            Some(PortEvent::DeltaThought("hm".into()))
        );
        assert_eq!(
            map_ka_event(
                Event::Delta {
                    kind: DeltaKind::Call {
                        tool: "bash".into(),
                        id: "c1".into(),
                    },
                },
                None,
                None
            ),
            None,
            "call-kind deltas are dropped (CallStarted carries them)"
        );
        assert_eq!(
            map_ka_event(
                Event::CallStarted {
                    tool: "bash".into(),
                    id: "c1".into(),
                    detail: "touch x".into(),
                },
                None,
                None
            ),
            Some(PortEvent::CallStarted {
                tool: "bash".into(),
                id: "c1".into(),
                detail: "touch x".into(),
            })
        );
        assert_eq!(
            map_ka_event(
                Event::CallFinished {
                    tool: "bash".into(),
                    id: "c1".into(),
                    ok: false,
                },
                None,
                None
            ),
            Some(PortEvent::CallFinished {
                tool: "bash".into(),
                id: "c1".into(),
                ok: false,
            })
        );
        assert_eq!(
            map_ka_event(
                Event::CallOutput {
                    tool: "bash".into(),
                    id: "c1".into(),
                    excerpt: "done".into(),
                    is_error: true,
                    spill: None,
                },
                None,
                None
            ),
            Some(PortEvent::CallOutput {
                tool: "bash".into(),
                id: "c1".into(),
                excerpt: "done".into(),
                is_error: true,
            })
        );
    }

    #[test]
    fn ask_maps_first_question_with_all_options() {
        let ev = Event::Ask {
            id: ka_protocol::AskId("ask-3".into()),
            questions: vec![
                AskQuestion {
                    text: "run it?".into(),
                    options: vec!["allow".into(), "always".into(), "deny".into()],
                    detail: Some("touch x".into()),
                },
                AskQuestion {
                    text: "second question dropped".into(),
                    options: vec!["no".into()],
                    detail: None,
                },
            ],
        };
        assert_eq!(
            map_ka_event(ev, None, None),
            Some(PortEvent::Ask {
                ask_id: "ask-3".into(),
                question: "run it?".into(),
                detail: Some("touch x".into()),
                options: vec!["allow".into(), "always".into(), "deny".into()],
            })
        );
    }

    #[test]
    fn turn_finished_sums_tokens_and_keeps_cost() {
        let ev = Event::TurnFinished {
            stop: Stop::Aborted,
            usage: Usage {
                input: 3,
                output: 5,
                cache_read: 7,
                cache_write: 11,
                cost: 0.5,
            },
        };
        assert_eq!(
            map_ka_event(ev, None, None),
            Some(PortEvent::TurnFinished {
                stop: "aborted".into(),
                cost_usd: 0.5,
                tokens: 26,
            })
        );
    }

    /// The three answers ka carries on `Event::Note` project by their
    /// marker prefix, and by nothing else: the engine's degrade notes
    /// and any other note stay `Note`s.
    #[test]
    fn note_markers_project_the_three_answers() {
        assert_eq!(
            map_ka_event(
                Event::Note {
                    message: "btw: the spill GC keeps the newest file because …".into(),
                },
                None,
                None
            ),
            Some(PortEvent::SideAnswer {
                text: "the spill GC keeps the newest file because …".into()
            })
        );
        let summary = "1 file(s), 3 +/- lines — full: spill://1730000000000-0\n\n+hi";
        assert_eq!(
            map_ka_event(
                Event::Note {
                    message: format!("session diff: {summary}"),
                },
                None,
                None
            ),
            Some(PortEvent::SessionDiff {
                summary: summary.into()
            })
        );
        assert_eq!(
            map_ka_event(
                Event::Note {
                    message: "session diff: no changes".into(),
                },
                None,
                None
            ),
            Some(PortEvent::SessionDiff {
                summary: "no changes".into()
            })
        );
        assert_eq!(
            map_ka_event(
                Event::Note {
                    message: "idle recap: Fixing the parser; tests pending.".into(),
                },
                None,
                None
            ),
            Some(PortEvent::Recap {
                text: "Fixing the parser; tests pending.".into()
            })
        );

        // The markers are whole words: a near-miss is an ordinary note.
        for plain in [
            "side question unavailable",
            "idle recap unavailable: no [roles] fast configured",
            "idle recap unavailable: fast role failed",
            "session diff",
            "a btw: answer inside a sentence",
            "idle_recap: underscored",
            "↩ recovered interrupted turn",
        ] {
            let expected = PortEvent::Note(plain.to_string());
            assert_eq!(
                map_ka_event(
                    Event::Note {
                        message: plain.to_string(),
                    },
                    None,
                    None
                ),
                Some(expected),
                "{plain:?}"
            );
        }
        // The unavailable reply stays a `Note` — and the marker a runtime
        // matches a pending `/btw` on is exactly that first note.
        assert!("side question unavailable".starts_with(SIDE_ANSWER_UNAVAILABLE));
    }

    #[test]
    fn context_breakdown_projects_name_and_tokens() {
        assert_eq!(
            map_ka_event(
                Event::ContextBreakdown {
                    parts: vec![
                        ka_protocol::ContextPart {
                            name: "system".into(),
                            tokens: 3_000,
                        },
                        ka_protocol::ContextPart {
                            name: "tools".into(),
                            tokens: 1_200,
                        },
                        ka_protocol::ContextPart {
                            name: "hint:shake".into(),
                            tokens: 40,
                        },
                    ],
                    window: 200_000,
                },
                None,
                None
            ),
            Some(PortEvent::ContextBreakdown {
                parts: vec![
                    ("system".into(), 3_000),
                    ("tools".into(), 1_200),
                    ("hint:shake".into(), 40),
                ],
                window: 200_000,
            })
        );
        // An unknown window is 0, not a dropped event.
        assert_eq!(
            map_ka_event(
                Event::ContextBreakdown {
                    parts: Vec::new(),
                    window: 0,
                },
                None,
                None
            ),
            Some(PortEvent::ContextBreakdown {
                parts: Vec::new(),
                window: 0,
            })
        );
    }

    #[test]
    fn note_idle_and_error_map_directly() {
        assert_eq!(map_ka_event(Event::Idle, None, None), Some(PortEvent::Idle));
        assert_eq!(
            map_ka_event(
                Event::Note {
                    message: "↩ recovered interrupted turn".into(),
                },
                None,
                None
            ),
            Some(PortEvent::Note("↩ recovered interrupted turn".into()))
        );
        assert_eq!(
            map_ka_event(
                Event::Error {
                    class: ka_protocol::ErrorClass::RateLimit,
                    retryable: true,
                    message: "slow down".into(),
                },
                None,
                None
            ),
            Some(PortEvent::EngineError {
                class: "rate_limit".into(),
                retryable: true,
                message: "slow down".into(),
            })
        );
    }

    #[test]
    fn unmapped_events_are_dropped() {
        for ev in [
            Event::SessionInfo { id: "s1".into() },
            Event::ModeChanged {
                mode: ka_protocol::Mode::Readonly,
            },
            Event::EffortChanged {
                level: ka_protocol::Effort::Medium,
            },
        ] {
            assert_eq!(map_ka_event(ev, None, None), None);
        }
        // ModelChanged is no longer dropped: the fallback hop is visible.
        assert_eq!(
            map_ka_event(
                Event::ModelChanged {
                    selector: "a/b".into(),
                },
                None,
                None,
            ),
            Some(PortEvent::ModelChanged("a/b".into()))
        );
    }

    #[test]
    fn plan_context_title_compacting_and_inventory_project() {
        assert_eq!(
            map_ka_event(
                Event::Todos {
                    items: vec![
                        ka_protocol::TodoItem {
                            text: "first".into(),
                            state: ka_protocol::TodoState::Pending,
                            confidence: None,
                        },
                        ka_protocol::TodoItem {
                            text: "second".into(),
                            state: ka_protocol::TodoState::Done,
                            confidence: Some(90),
                        },
                    ],
                },
                None,
                None
            ),
            Some(PortEvent::Plan(vec![
                ("first".into(), "pending".into()),
                ("second".into(), "done".into()),
            ]))
        );
        assert_eq!(
            map_ka_event(Event::ContextMeter { used: 9, window: 0 }, None, None),
            Some(PortEvent::Context { used: 9, window: 0 })
        );
        assert_eq!(
            map_ka_event(Event::Title { title: "t".into() }, None, None),
            Some(PortEvent::Title("t".into()))
        );
        assert_eq!(
            map_ka_event(Event::DigestStarted, None, None),
            Some(PortEvent::Compacting { done: false })
        );
        assert_eq!(
            map_ka_event(
                Event::DigestFinished {
                    kept: ka_protocol::RecordId("r7".into()),
                },
                None,
                None
            ),
            Some(PortEvent::Compacting { done: true })
        );
        assert_eq!(
            map_ka_event(
                Event::Inventory {
                    tools: vec!["read".into()],
                    mcp: Vec::new(),
                    agents: Vec::new(),
                    skills: Vec::new(),
                    prompts: Vec::new(),
                    disabled: Vec::new(),
                },
                None,
                None
            ),
            Some(PortEvent::Inventory {
                tools: vec!["read".into()]
            })
        );
    }
}
