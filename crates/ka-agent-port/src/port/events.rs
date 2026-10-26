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
}

/// Map one ka event to its port projection; `None` drops it. Dropped by
/// design: `session_info` (captured as strand identity), the bootstrap
/// state echo (`model_changed`, `mode_changed`, `effort_changed`,
/// `features_changed`), `inventory`, `todos`, `title`, `context_meter`,
/// `context_breakdown`, `digest_*`, call-kind deltas, and every
/// surface-specific event. Bootstrap state is verified by contract
/// tests, not streamed upward.
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
        E::Note { message } => PortEvent::Note(message),
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
            Event::ModelChanged {
                selector: "a/b".into(),
            },
            Event::ModeChanged {
                mode: ka_protocol::Mode::Plan,
            },
            Event::Title { title: "t".into() },
            Event::ContextMeter { used: 1, window: 2 },
            Event::DigestStarted,
        ] {
            assert_eq!(map_ka_event(ev, None, None), None);
        }
    }
}
