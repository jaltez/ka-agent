//! The scripted speaker: deterministic, network-free model responses for
//! contract tests and e2e runs (`ka_engine::spawn_with_speaker`).
//!
//! A scenario is a JSON file:
//!
//! ```json
//! {
//!   "steps": [
//!     {"text": "hi"},
//!     {"thought": "hmm"},
//!     {"sleep_ms": 800},
//!     {"call": {"tool": "bash", "args": {"command": "touch x"}}},
//!     {"fail": {"class": "auth", "retryable": false, "message": "bad key"}}
//!   ],
//!   "stop": "done",
//!   "cost_usd": 0.0,
//!   "tokens": 0
//! }
//! ```
//!
//! Each model request **drains the remaining steps in order**: text,
//! thought and call deltas first, then one terminal event — `Finished`
//! (`stop` + usage) when the queue runs dry, or the first `fail` step's
//! `Failed` (the steps after a `fail` wait for the next request, so a
//! retryable failure can be followed by a scripted recovery). The engine
//! loops requests within one turn (tool results feed back in), so a
//! `call` step's result — allow or deny — is observed by the request
//! that follows it. A `sleep_ms` step pauses before the next step, which
//! is how a slow (still running) turn is scripted. `tokens` count as
//! input tokens; the engine recomputes cost from the catalog row.

use std::collections::VecDeque;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use ka_dialect::speaker::{SpeakFuture, SpeakRequest, Speaker, StreamEvent, ToolCall};
use ka_protocol::{ErrorClass, Stop, Usage};
use serde::Deserialize;

/// Parse a `stop` word (`done|length|aborted|error`).
pub fn parse_stop(s: &str) -> Option<Stop> {
    match s {
        "done" => Some(Stop::Done),
        "length" => Some(Stop::Length),
        "aborted" => Some(Stop::Aborted),
        "error" => Some(Stop::Error),
        _ => None,
    }
}

/// Parse an `ErrorClass` word
/// (`auth|rate_limit|overflow|network|protocol|unsupported|internal`).
pub fn parse_error_class(s: &str) -> Option<ErrorClass> {
    match s {
        "auth" => Some(ErrorClass::Auth),
        "rate_limit" => Some(ErrorClass::RateLimit),
        "overflow" => Some(ErrorClass::Overflow),
        "network" => Some(ErrorClass::Network),
        "protocol" => Some(ErrorClass::Protocol),
        "unsupported" => Some(ErrorClass::Unsupported),
        "internal" => Some(ErrorClass::Internal),
        _ => None,
    }
}

/// One scripted model-output step.
#[derive(Debug, Clone, PartialEq)]
pub enum Step {
    /// Visible text delta.
    Text(String),
    /// Reasoning/thought delta.
    Thought(String),
    /// A complete tool call; `arguments` must be a JSON object the named
    /// tool accepts (bash: `{"command": "..."}`, see ka's
    /// `hands/bash.rs` parameter schema).
    Call {
        /// Tool name.
        tool: String,
        /// Parsed tool arguments.
        arguments: serde_json::Value,
    },
    /// Pause before the next step. Models take time; a slow turn is what
    /// makes "a message that arrives mid-turn queues" testable without
    /// racing the engine's idle signal.
    Sleep {
        /// Milliseconds to wait before the next step is emitted.
        ms: u64,
    },
    /// Terminal wire failure.
    Fail {
        /// Error classification.
        class: ErrorClass,
        /// Whether a retry could help.
        retryable: bool,
        /// Human-readable detail.
        message: String,
    },
}

/// A parsed scenario: the script plus the terminal stop/usage block.
#[derive(Debug, Clone, PartialEq)]
pub struct Scenario {
    steps: Vec<Step>,
    stop: Stop,
    cost_usd: f64,
    tokens: u64,
}

impl Scenario {
    /// Load and parse a scenario file.
    pub fn load(path: &Path) -> Result<Self, String> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| format!("scenario {}: {e}", path.display()))?;
        Self::from_json(&text).map_err(|e| format!("scenario {}: {e}", path.display()))
    }

    /// Parse scenario JSON.
    pub fn from_json(text: &str) -> Result<Self, String> {
        #[derive(Deserialize)]
        struct ScenarioFile {
            #[serde(default)]
            steps: Vec<StepSpec>,
            #[serde(default = "default_stop")]
            stop: String,
            #[serde(default)]
            cost_usd: f64,
            #[serde(default)]
            tokens: u64,
        }
        #[derive(Deserialize)]
        struct CallSpec {
            tool: String,
            args: serde_json::Value,
        }
        #[derive(Deserialize)]
        struct FailSpec {
            class: String,
            #[serde(default)]
            retryable: bool,
            message: String,
        }
        // One single-key object per step; untagged tries each shape in
        // order and the keys are disjoint.
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum StepSpec {
            Text { text: String },
            Thought { thought: String },
            Sleep { sleep_ms: u64 },
            Call { call: CallSpec },
            Fail { fail: FailSpec },
        }
        fn default_stop() -> String {
            "done".to_string()
        }

        let file: ScenarioFile =
            serde_json::from_str(text).map_err(|e| format!("invalid scenario JSON: {e}"))?;
        let stop = parse_stop(&file.stop)
            .ok_or_else(|| format!("unknown stop {:?} (done|length|aborted|error)", file.stop))?;
        let mut steps = Vec::with_capacity(file.steps.len());
        for spec in file.steps {
            let step = match spec {
                StepSpec::Text { text } => Step::Text(text),
                StepSpec::Thought { thought } => Step::Thought(thought),
                StepSpec::Sleep { sleep_ms } => Step::Sleep { ms: sleep_ms },
                StepSpec::Call { call } => {
                    if !call.args.is_object() {
                        return Err(format!(
                            "call for tool {:?}: args must be a JSON object",
                            call.tool
                        ));
                    }
                    Step::Call {
                        tool: call.tool,
                        arguments: call.args,
                    }
                }
                StepSpec::Fail { fail } => Step::Fail {
                    class: parse_error_class(&fail.class).ok_or_else(|| {
                        format!(
                            "unknown error class {:?} \
                             (auth|rate_limit|overflow|network|protocol|unsupported|internal)",
                            fail.class
                        )
                    })?,
                    retryable: fail.retryable,
                    message: fail.message,
                },
            };
            steps.push(step);
        }
        Ok(Scenario {
            steps,
            stop,
            cost_usd: file.cost_usd,
            tokens: file.tokens,
        })
    }

    /// The scripted steps (consumed left to right across requests).
    pub fn steps(&self) -> &[Step] {
        &self.steps
    }
}

/// A [`Speaker`] that plays one [`Scenario`]: each request drains the
/// remaining steps in order and then finishes, or fails terminally on a
/// `fail` step. No network, no provider.
pub struct ScenarioSpeaker {
    steps: Arc<Mutex<VecDeque<Step>>>,
    stop: Stop,
    cost_usd: f64,
    tokens: u64,
    calls: AtomicU64,
}

impl ScenarioSpeaker {
    /// Build the speaker from a parsed scenario.
    pub fn new(scenario: Scenario) -> Self {
        Self {
            steps: Arc::new(Mutex::new(scenario.steps.into_iter().collect())),
            stop: scenario.stop,
            cost_usd: scenario.cost_usd,
            tokens: scenario.tokens,
            calls: AtomicU64::new(0),
        }
    }
}

impl Speaker for ScenarioSpeaker {
    fn speak<'a>(
        &'a self,
        _req: SpeakRequest,
        out: tokio::sync::mpsc::Sender<StreamEvent>,
    ) -> SpeakFuture<'a> {
        Box::pin(async move {
            loop {
                let next = self.steps.lock().ok().and_then(|mut q| q.pop_front());
                let event = match next {
                    Some(Step::Text(text)) => StreamEvent::Text(text),
                    Some(Step::Thought(text)) => StreamEvent::Thought(text),
                    Some(Step::Sleep { ms }) => {
                        tokio::time::sleep(std::time::Duration::from_millis(ms)).await;
                        continue;
                    }
                    Some(Step::Call { tool, arguments }) => {
                        let n = self.calls.fetch_add(1, Ordering::Relaxed) + 1;
                        StreamEvent::Call(ToolCall {
                            id: format!("scn-{n}"),
                            tool,
                            arguments,
                        })
                    }
                    // a fail step is terminal for this request
                    Some(Step::Fail {
                        class,
                        retryable,
                        message,
                    }) => {
                        if out
                            .send(StreamEvent::Failed {
                                class,
                                retryable,
                                message,
                            })
                            .await
                            .is_err()
                        {
                            return;
                        }
                        return;
                    }
                    None => StreamEvent::Finished {
                        stop: self.stop,
                        usage: Usage {
                            input: self.tokens,
                            cost: self.cost_usd,
                            ..Usage::default()
                        },
                    },
                };
                let finished = matches!(event, StreamEvent::Finished { .. });
                if out.send(event).await.is_err() {
                    return;
                }
                if finished {
                    return;
                }
            }
        })
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    fn drain(rx: &mut tokio::sync::mpsc::Receiver<StreamEvent>) -> Vec<StreamEvent> {
        let mut out = Vec::new();
        while let Ok(ev) = rx.try_recv() {
            out.push(ev);
        }
        out
    }

    #[test]
    fn parses_the_documented_shape() {
        let scen = Scenario::from_json(
            r#"{
                "steps": [
                    {"text": "hi"},
                    {"thought": "hmm"},
                    {"sleep_ms": 250},
                    {"call": {"tool": "bash", "args": {"command": "touch x"}}},
                    {"fail": {"class": "auth", "retryable": false, "message": "bad key"}}
                ],
                "stop": "aborted",
                "cost_usd": 0.5,
                "tokens": 9
            }"#,
        )
        .unwrap();
        assert_eq!(
            scen.steps(),
            &[
                Step::Text("hi".into()),
                Step::Thought("hmm".into()),
                Step::Sleep { ms: 250 },
                Step::Call {
                    tool: "bash".into(),
                    arguments: serde_json::json!({"command": "touch x"}),
                },
                Step::Fail {
                    class: ErrorClass::Auth,
                    retryable: false,
                    message: "bad key".into(),
                },
            ]
        );
    }

    #[tokio::test]
    async fn defaults_are_stop_done_empty_steps() {
        let scen = Scenario::from_json("{}").unwrap();
        assert!(scen.steps().is_empty());
        let speaker = ScenarioSpeaker::new(scen);
        let (tx, mut rx) = tokio::sync::mpsc::channel(16);
        speaker.speak(speak_req(), tx).await;
        assert_eq!(
            drain(&mut rx),
            vec![StreamEvent::Finished {
                stop: Stop::Done,
                usage: Usage::default(),
            }]
        );
    }

    #[test]
    fn rejects_unknown_stop_class_and_non_object_args() {
        assert!(Scenario::from_json(r#"{"stop": "boom"}"#).is_err());
        assert!(Scenario::from_json(r#"[{"fail":{"class":"nope","message":"x"}}]"#).is_err());
        assert!(
            Scenario::from_json(r#"{"steps":[{"call":{"tool":"bash","args":"not an object"}}]}"#)
                .is_err()
        );
    }

    /// A minimal SpeakRequest with the scenario dialect (`Dialect` has no
    /// `Default` — the scenario catalog row is the honest way to build
    /// one).
    fn speak_req() -> SpeakRequest {
        let dialect = ka_dialect::Catalog::parse(
            "[dialects.\"scen/m\"]\nwire = \"openai_chat\"\nbase_url = \"http://127.0.0.1:1\"\ncontext = 1000000\n",
        )
        .unwrap()
        .get("scen/m")
        .cloned()
        .unwrap();
        SpeakRequest {
            model_id: "scen/m".into(),
            dialect,
            effort: None,
            system: String::new(),
            messages: Vec::new(),
            tools: Vec::new(),
            token: None,
            cache_key: None,
            schema: None,
            native_search: false,
            session_id: None,
        }
    }

    #[tokio::test]
    async fn steps_stream_in_order_then_finish() {
        let scen = Scenario::from_json(
            r#"{"steps":[{"text":"a"},{"thought":"b"},
                {"call":{"tool":"bash","args":{"command":"echo one"}}},
                {"call":{"tool":"bash","args":{"command":"echo two"}}}],
               "stop":"done","tokens":42,"cost_usd":0.001}"#,
        )
        .unwrap();
        let speaker = ScenarioSpeaker::new(scen);
        let (tx, mut rx) = tokio::sync::mpsc::channel(16);
        speaker.speak(speak_req(), tx).await;
        let events = drain(&mut rx);
        assert_eq!(events.len(), 5, "{events:?}");
        assert_eq!(events[0], StreamEvent::Text("a".into()));
        assert_eq!(events[1], StreamEvent::Thought("b".into()));
        assert!(
            matches!(&events[2], StreamEvent::Call(c) if c.tool == "bash"
            && c.arguments == serde_json::json!({"command": "echo one"}))
        );
        assert!(matches!(&events[3], StreamEvent::Call(c) if c.id == "scn-2"));
        assert_eq!(
            events[4],
            StreamEvent::Finished {
                stop: Stop::Done,
                usage: Usage {
                    input: 42,
                    cost: 0.001,
                    ..Usage::default()
                },
            }
        );
    }

    #[tokio::test]
    async fn fail_step_is_terminal_and_queue_dries_across_requests() {
        let scen = Scenario::from_json(
            r#"{"steps":[{"call":{"tool":"bash","args":{"command":"touch x"}}},
                {"fail":{"class":"rate_limit","retryable":true,"message":"slow down"}}],
               "stop":"length"}"#,
        )
        .unwrap();
        let speaker = ScenarioSpeaker::new(scen);
        let (tx, mut rx) = tokio::sync::mpsc::channel(16);
        // request 1 drains the queue until the fail step: call, then the
        // terminal Failed (no Finished after a failure)
        speaker.speak(speak_req(), tx.clone()).await;
        let events = drain(&mut rx);
        assert_eq!(events.len(), 2, "{events:?}");
        assert!(matches!(&events[0], StreamEvent::Call(c) if c.tool == "bash"));
        assert_eq!(
            events[1],
            StreamEvent::Failed {
                class: ErrorClass::RateLimit,
                retryable: true,
                message: "slow down".into(),
            }
        );
        // request 2: queue dry — immediate finish with the scenario stop
        speaker.speak(speak_req(), tx).await;
        assert_eq!(
            drain(&mut rx),
            vec![StreamEvent::Finished {
                stop: Stop::Length,
                usage: Usage::default(),
            }]
        );
    }
}
