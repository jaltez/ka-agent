//! One serde event model for everything the runtime tells a surface.
//!
//! Every outbound occurrence is an [`AgentEvent`] wrapped in a sequenced
//! [`Envelope`]; surfaces implement one method ([`crate::surface::Surface::emit`])
//! and pick the variants they render. `Hello` and `Reply` are per-connection
//! events synthesized by attach servers — the runtime never emits them.

use serde::{Deserialize, Serialize};

use crate::status::StatusReport;

/// One plan/todo item as the engine reports it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PlanItem {
    pub text: String,
    pub state: String,
}

/// Everything the runtime can tell a surface, serde-tagged `"ev"`
/// (snake_case) — one JSON object per line on every wire (NDJSON attach,
/// SSE, the ring replay).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "ev", rename_all = "snake_case")]
pub enum AgentEvent {
    /// First event on an attach connection (per-connection; never
    /// broadcast).
    Hello { report: Box<StatusReport> },
    /// A router reply for one client (per-connection; never broadcast).
    Reply { text: String },
    /// An inbound message accepted from any channel (for cross-channel
    /// transcripts).
    Inbound { source: String, text: String },
    /// A prompt cycle started: the turn id (may be empty when the engine
    /// reports the start before the submit handshake records one) and who
    /// started it (`console`, `telegram`, `cron`, …).
    TurnStarted { turn: String, source: String },
    /// Streamed assistant text.
    Delta { text: String },
    /// Streamed reasoning. The runtime does not buffer it into the final
    /// answer; surfaces may drop it.
    Thought { text: String },
    /// A tool call began.
    ToolStarted {
        id: String,
        tool: String,
        detail: String,
    },
    /// A tool call finished; `error` is the first line of its output when
    /// the call failed.
    ToolFinished {
        id: String,
        tool: String,
        ok: bool,
        ms: u64,
        error: Option<String>,
    },
    /// The model's live plan (whole-list replacement).
    Plan { items: Vec<PlanItem> },
    /// Context meter (emitted only when the integer percentage changes).
    Context { used: u64, window: u64 },
    /// A context digest (compaction) started (`done: false`) or finished.
    Compacting { done: bool },
    /// In-turn progress or diagnostics; a surface may fold or drop these.
    Note { text: String },
    /// A message the operator must receive as its own message (reminders,
    /// cron output, pauses, recovery, queue state).
    Notice { text: String },
    /// A reminder fired (a notification, never a model turn).
    Reminder { id: String, text: String },
    /// A new approval is pending; render the card.
    Approval {
        id: String,
        question: String,
        detail: Option<String>,
        options: Vec<String>,
        expires_at_ms: i64,
    },
    /// A pending approval settled (decision, expiry or cancellation).
    ApprovalSettled { id: String, status: String },
    /// The model proposed a memory (`memory_propose`): offer the operator
    /// a one-tap keep/discard.
    MemoryProposal { id: String, text: String },
    /// The model proposed a skill; offer accept/reject.
    SkillProposal {
        id: String,
        name: String,
        description: String,
    },
    /// A file for the operator.
    File {
        path: String,
        name: String,
        caption: String,
    },
    /// One chunk of the final answer at the `idle` delivery boundary.
    FinalChunk { index: u32, text: String },
    /// The turn settled. `text` is the final answer capped at 4 KiB.
    TurnFinished {
        stop: String,
        cost_usd: f64,
        tokens: u64,
        ms: u64,
        text: String,
    },
    /// A scheduled job's run settled.
    CronFinished { id: String, status: String },
    /// How deep the durable queue is after an enqueue.
    Queue { depth: u32 },
    /// Model/mode/effort changed (any field may be absent).
    Settings {
        model: Option<String>,
        mode: Option<String>,
        effort: Option<String>,
    },
    /// The session's status moved.
    Session { status: String },
    /// Engine-level error (class + human message); presentation only.
    Error { class: String, message: String },
}

/// One sequenced event on the wire: a monotonically increasing `seq`
/// (per runtime, starting at 1) and the wall clock it was emitted at.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Envelope {
    pub seq: u64,
    pub at_ms: i64,
    #[serde(flatten)]
    pub ev: AgentEvent,
}

impl Envelope {
    /// Wrap `ev` with the given sequence number and `now` as `at_ms`.
    pub fn new(seq: u64, ev: AgentEvent) -> Self {
        Self {
            seq,
            at_ms: ka_agent_core::now_ms(),
            ev,
        }
    }
}

/// Cap for [`AgentEvent::TurnFinished`] text.
pub const TURN_TEXT_MAX: usize = 4096;

/// A turn's final text as carried by [`AgentEvent::TurnFinished`] (capped
/// at 4 KiB on a char boundary).
pub fn turn_text(text: &str) -> String {
    ka_agent_core::outbox::truncate_at_char(text, TURN_TEXT_MAX).to_string()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[test]
    fn envelopes_serialize_with_snake_case_tag_and_flattened_payload() {
        let env = Envelope::new(7, AgentEvent::Delta { text: "hi".into() });
        let v = serde_json::to_value(&env).unwrap();
        assert_eq!(
            v,
            serde_json::json!({"seq": 7, "at_ms": env.at_ms, "ev": "delta", "text": "hi"})
        );
        let back: Envelope = serde_json::from_value(v).unwrap();
        assert_eq!(back, env);
    }

    #[test]
    fn turn_text_caps_at_4k_bytes_on_a_char_boundary() {
        let long = "α".repeat(5000);
        assert!(turn_text(&long).len() <= TURN_TEXT_MAX);
        assert!(turn_text(&long).chars().all(|c| c == 'α'));
        assert_eq!(turn_text("short"), "short");
    }
}
