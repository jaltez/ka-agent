//! The channel-agnostic output surface. Telegram implements this in the next
//! milestone; the local chat CLI implements it now. Implementations must be
//! cheap and non-blocking — they render, they never decide.

use ka_agent_core::{Approval, ApprovalStatus, SessionStatus};

/// Everything a surface needs to render one approval card (SEED.md §5.5).
#[derive(Debug, Clone)]
pub struct ApprovalCard {
    pub approval_id: String,
    pub question: String,
    pub detail: Option<String>,
    pub options: Vec<String>,
    pub expires_at_ms: i64,
}

impl ApprovalCard {
    pub fn from_approval(a: &Approval) -> Option<Self> {
        match &a.kind {
            ka_agent_core::ApprovalKind::KaAsk {
                question,
                detail,
                options,
                ..
            } => Some(Self {
                approval_id: a.id.to_string(),
                question: question.clone(),
                detail: detail.clone(),
                options: options.clone(),
                expires_at_ms: a.expires_at_ms,
            }),
            ka_agent_core::ApprovalKind::Intent {
                tool,
                args_summary,
                effect,
            } => Some(Self {
                approval_id: a.id.to_string(),
                question: format!("{effect}: `{tool}` {args_summary}"),
                detail: None,
                options: vec!["allow".into(), "deny".into()],
                expires_at_ms: a.expires_at_ms,
            }),
        }
    }
}

pub trait Surface: Send + Sync {
    /// A prompt cycle started.
    fn on_turn_started(&self) {}

    /// Streamed assistant text. Surfaces throttle; the runtime sends every
    /// delta it intends to surface.
    fn on_delta_text(&self, _text: &str) {}

    /// Streamed reasoning. Optional to render; the runtime does not buffer
    /// it into the final answer.
    fn on_delta_thought(&self, _text: &str) {}

    /// Informational note (compaction, recovery, queue state, ...).
    fn on_note(&self, text: &str);

    /// A new approval is pending; render the card.
    fn on_approval(&self, card: &ApprovalCard);

    /// A pending approval settled (decision, expiry or cancellation).
    fn on_approval_settled(&self, approval_id: &str, status: ApprovalStatus);

    /// One chunk of the final answer at the `idle` delivery boundary
    /// (SEED.md §3.7). Channel surfaces that do not stream live text send
    /// these; streaming surfaces may ignore them.
    fn on_final_chunk(&self, _index: u32, _text: &str) {}

    fn on_turn_finished(&self, stop: &str, cost_usd: f64, tokens: u64);

    fn on_session_status(&self, status: SessionStatus);

    /// Engine-level error (class + human message) — SEED.md §3.8 reactions
    /// are the runtime's job; this is presentation only.
    fn on_error(&self, class: &str, message: &str);
}
