//! The daemon ↔ worker NDJSON envelope (`docs/contract/worker-protocol.md`).
//!
//! One JSON object per line: [`WorkerCommand`] on the worker's stdin,
//! [`WorkerEvent`] on its stdout. stderr carries human diagnostics only and
//! is never parsed as protocol. Inner engine traffic rides
//! [`WorkerEvent::Ka`] as the verbatim serde form of [`ka_protocol::Event`].

use std::fmt;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// One attached image: base64 bytes plus IANA media type.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImagePayload {
    /// Base64-encoded image bytes.
    pub data_base64: String,
    /// Media type, e.g. `image/png`.
    pub media_type: String,
}

/// Which strand `Open` attaches to.
///
/// Wire form: a **plain JSON string** — the sentinel `"new"` starts a fresh
/// strand, any other string is a strand file path (the same convention as
/// ka's own `switch_strand { id }` command). A path literally equal to
/// `"new"` is therefore not addressable; no real strand path can be that
/// short.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StrandSel {
    /// Create a fresh strand (`StrandChoice::New`).
    New,
    /// Resume the strand at this file path (`StrandChoice::Path`).
    Path(String),
}

impl Serialize for StrandSel {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            StrandSel::New => serializer.serialize_str("new"),
            StrandSel::Path(p) => serializer.serialize_str(p),
        }
    }
}

impl<'de> Deserialize<'de> for StrandSel {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Visitor;
        impl serde::de::Visitor<'_> for Visitor {
            type Value = StrandSel;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(r#"the strand sentinel "new" or a strand file path"#)
            }
            fn visit_str<E: serde::de::Error>(self, s: &str) -> Result<StrandSel, E> {
                if s == "new" {
                    Ok(StrandSel::New)
                } else if s.is_empty() {
                    Err(E::custom("empty strand selector"))
                } else {
                    Ok(StrandSel::Path(s.to_string()))
                }
            }
        }
        deserializer.deserialize_str(Visitor)
    }
}

/// Daemon → worker ops, serde-tagged `"op"` (snake_case), one JSON object
/// per stdin line.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum WorkerCommand {
    /// Start or attach an engine on one strand.
    Open {
        /// Working directory (also the strand-storage key).
        workspace: PathBuf,
        /// The `"new"` sentinel or a strand file path.
        strand: StrandSel,
        /// Model selector to re-assert after attach (validation F-1).
        #[serde(default)]
        model: Option<String>,
        /// Permission mode to re-assert after attach.
        #[serde(default)]
        mode: Option<String>,
        /// Ka engine layer TOML (ka's strict `ka.toml` grammar), applied
        /// below the worker's security overlay.
        #[serde(default)]
        engine_layer_toml: Option<String>,
        /// Scripted-speaker scenario file (tests and e2e; `null` in
        /// production).
        #[serde(default)]
        scenario: Option<PathBuf>,
        /// Exact tool names that never ask (the approval profile: reads,
        /// proposals and local writes). Inserted as allow rules ahead of
        /// every other rule.
        #[serde(default)]
        auto_allow: Vec<String>,
    },
    /// One prompt: text plus optional images and per-turn skills.
    Submit {
        /// Prompt text.
        text: String,
        /// Per-turn model override (background turns use the background
        /// model; `None` keeps the session model).
        #[serde(default)]
        model: Option<String>,
        /// Images attached to the prompt.
        #[serde(default)]
        images: Vec<ImagePayload>,
        /// Skill names to inject for this turn.
        #[serde(default)]
        skills: Vec<String>,
    },
    /// Resolve a pending ask; `choice` indexes the ask's options.
    Answer {
        /// The ask being answered.
        ask_id: String,
        /// Chosen option index (0 allow · 1 always · 2 deny for engine
        /// permission asks).
        choice: usize,
    },
    /// Cancel the running cycle.
    Abort,
    /// Switch the model.
    SetModel {
        /// Model selector, e.g. `vendor/model:effort`.
        selector: String,
    },
    /// Switch the permission mode.
    SetMode {
        /// `ask` | `accept_edits` | `allow` | `readonly` (the old
        /// `guarded`/`free`/`plan` spellings are accepted as aliases).
        mode: String,
    },
    /// Per-turn reasoning effort (`off|low|medium|high|max`; empty =
    /// session default).
    SetEffort {
        /// The effort level, or "" to clear.
        level: String,
    },
    /// Mid-turn steering: delivered to the engine's live turn.
    Steer {
        /// What the operator added.
        text: String,
    },
    /// Undo the most recent file change of this session.
    UndoFile,
    /// Name (title) the session.
    SetName {
        /// The alias (1–64 chars, no whitespace).
        name: String,
    },
    /// Compact the strand into a digest.
    Compact,
    /// Close the engine and exit cleanly.
    Close,
}

/// Worker → daemon events, serde-tagged `"ev"` (snake_case), one JSON
/// object per stdout line.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "ev", rename_all = "snake_case")]
pub enum WorkerEvent {
    /// Worker is up; `open` has not been read yet.
    Ready,
    /// The active strand (from ka's `session_info`): id plus the resolved
    /// file path. The path is `None` while the strand file does not exist
    /// on disk yet — ka materializes a fresh strand only on its first
    /// append, so a just-opened new strand has an id but no file.
    Strand {
        /// Strand id (ka `session_info.id`).
        id: String,
        /// Resolved strand file path, when it exists on disk.
        path: Option<String>,
    },
    /// One verbatim ka engine event.
    Ka {
        /// The event, exactly as ka serialized it.
        event: ka_protocol::Event,
    },
    /// The engine event channel closed (with reason, when known).
    EngineClosed {
        /// Why it closed, when the worker could tell.
        reason: Option<String>,
    },
    /// Unrecoverable worker error; the worker exits right after.
    Fatal {
        /// Human-readable error.
        message: String,
    },
}

/// Serialize one NDJSON line (trailing newline included).
pub fn to_line<T: Serialize>(value: &T) -> Result<String, serde_json::Error> {
    ka_protocol::to_line(value)
}

/// Parse one NDJSON line.
pub fn from_line<T: for<'de> Deserialize<'de>>(line: &str) -> Result<T, serde_json::Error> {
    ka_protocol::from_line(line)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    /// `ka_protocol::Event` is not `PartialEq`, so round-trips through it
    /// are compared as serialized JSON.
    fn round_trip<T: Serialize + for<'de> Deserialize<'de>>(value: &T) {
        let first = serde_json::to_string(value).unwrap();
        let back: T = from_line(&first).unwrap();
        let second = serde_json::to_string(&back).unwrap();
        assert_eq!(first, second);
    }

    #[test]
    fn strand_sel_wire_form() {
        round_trip(&StrandSel::New);
        round_trip(&StrandSel::Path("/srv/ka/strands/s1-ab.jsonl".into()));
        assert_eq!(serde_json::to_string(&StrandSel::New).unwrap(), r#""new""#);
        assert_eq!(
            serde_json::to_string(&StrandSel::Path("/a/b.jsonl".into())).unwrap(),
            r#""/a/b.jsonl""#
        );
        let open: WorkerCommand =
            from_line(r#"{"op":"open","workspace":"/w","strand":"new","scenario":null}"#).unwrap();
        assert_eq!(
            open,
            WorkerCommand::Open {
                auto_allow: Vec::new(),
                workspace: "/w".into(),
                strand: StrandSel::New,
                model: None,
                mode: None,
                engine_layer_toml: None,
                scenario: None,
            }
        );
    }

    #[test]
    fn empty_strand_selector_rejected() {
        assert!(from_line::<StrandSel>(r#""""#).is_err());
        assert!(from_line::<StrandSel>("7").is_err());
    }

    #[test]
    fn every_command_round_trips() {
        round_trip(&WorkerCommand::Open {
            auto_allow: Vec::new(),
            workspace: "/home/op/work".into(),
            strand: StrandSel::Path("/data/strands/s1-x.jsonl".into()),
            model: Some("anthropic/claude-sonnet-4-5".into()),
            mode: Some("ask".into()),
            engine_layer_toml: Some("model = \"a/b\"\n".into()),
            scenario: Some("/tmp/scen.json".into()),
        });
        round_trip(&WorkerCommand::Open {
            auto_allow: Vec::new(),
            workspace: "/w".into(),
            strand: StrandSel::New,
            model: None,
            mode: None,
            engine_layer_toml: None,
            scenario: None,
        });
        round_trip(&WorkerCommand::Submit {
            model: None,
            text: "fix the failing test".into(),
            images: vec![ImagePayload {
                data_base64: "aGk=".into(),
                media_type: "image/png".into(),
            }],
            skills: vec!["review".into()],
        });
        // images/skills default when absent
        let bare: WorkerCommand = from_line(r#"{"op":"submit","text":"hi"}"#).unwrap();
        assert_eq!(
            bare,
            WorkerCommand::Submit {
                model: None,
                text: "hi".into(),
                images: Vec::new(),
                skills: Vec::new(),
            }
        );
        round_trip(&WorkerCommand::Answer {
            ask_id: "ask-1".into(),
            choice: 2,
        });
        round_trip(&WorkerCommand::Abort);
        round_trip(&WorkerCommand::SetModel {
            selector: "openai/gpt-5.2".into(),
        });
        round_trip(&WorkerCommand::SetMode {
            mode: "readonly".into(),
        });
        round_trip(&WorkerCommand::SetName {
            name: "CI flake hunt".into(),
        });
        round_trip(&WorkerCommand::Compact);
        round_trip(&WorkerCommand::Close);
    }

    #[test]
    fn command_tags_are_snake_case_ops() {
        assert_eq!(
            serde_json::to_string(&WorkerCommand::Abort).unwrap(),
            r#"{"op":"abort"}"#
        );
        assert_eq!(
            serde_json::to_string(&WorkerCommand::Close).unwrap(),
            r#"{"op":"close"}"#
        );
        let set_mode = serde_json::to_string(&WorkerCommand::SetMode {
            mode: "allow".into(),
        })
        .unwrap();
        assert!(set_mode.starts_with(r#"{"op":"set_mode""#), "{set_mode}");
    }

    #[test]
    fn every_event_round_trips() {
        round_trip(&WorkerEvent::Ready);
        round_trip(&WorkerEvent::Strand {
            id: "s1-abc".into(),
            path: Some("/data/strands/s1-abc.jsonl".into()),
        });
        round_trip(&WorkerEvent::Strand {
            id: "s1-abc".into(),
            path: None,
        });
        round_trip(&WorkerEvent::Ka {
            event: ka_protocol::Event::Replay {
                messages: vec![ka_protocol::ReplayedMessage {
                    role: "user".into(),
                    content: "hello".into(),
                    digest: false,
                    thinking: None,
                    calls: Vec::new(),
                }],
            },
        });
        round_trip(&WorkerEvent::Ka {
            event: ka_protocol::Event::Delta {
                kind: ka_protocol::DeltaKind::Text("Hello".into()),
            },
        });
        round_trip(&WorkerEvent::Ka {
            event: ka_protocol::Event::TurnFinished {
                stop: ka_protocol::Stop::Error,
                usage: ka_protocol::Usage {
                    input: 3,
                    output: 5,
                    cache_read: 7,
                    cache_write: 11,
                    cost: 0.25,
                },
            },
        });
        round_trip(&WorkerEvent::Ka {
            event: ka_protocol::Event::Ask {
                id: ka_protocol::AskId("ask-2".into()),
                questions: vec![ka_protocol::AskQuestion {
                    text: "run `touch x`?".into(),
                    options: vec!["allow".into(), "always".into(), "deny".into()],
                    detail: None,
                }],
            },
        });
        round_trip(&WorkerEvent::EngineClosed { reason: None });
        round_trip(&WorkerEvent::EngineClosed {
            reason: Some("engine task exited without idle".into()),
        });
        round_trip(&WorkerEvent::Fatal {
            message: "open strand: record decode error".into(),
        });
    }

    #[test]
    fn event_tags_are_snake_case_evs() {
        assert_eq!(
            serde_json::to_string(&WorkerEvent::Ready).unwrap(),
            r#"{"ev":"ready"}"#
        );
        let ka = serde_json::to_string(&WorkerEvent::Ka {
            event: ka_protocol::Event::Idle,
        })
        .unwrap();
        assert_eq!(ka, r#"{"ev":"ka","event":{"type":"idle"}}"#);
    }

    #[test]
    fn to_line_appends_exactly_one_newline() {
        let line = to_line(&WorkerCommand::Abort).unwrap();
        assert!(line.ends_with("}\n"));
        assert_eq!(line.matches('\n').count(), 1);
        let back: WorkerCommand = from_line(&line).unwrap();
        assert_eq!(back, WorkerCommand::Abort);
    }
}
