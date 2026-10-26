//! The channel-agnostic output surface. Every runtime occurrence arrives as
//! one sequenced [`Envelope`]; implementations must be cheap and
//! non-blocking — they render, they never decide.

use ka_agent_core::Approval;

/// Everything a surface needs to render one approval card (SEED.md §5.5):
/// the payload of [`crate::event::AgentEvent::Approval`], kept for the
/// listing surfaces (`/approvals`, the web panel) that render pending
/// cards straight from the store.
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

    /// The broadcast form of this card.
    pub fn to_event(&self) -> crate::event::AgentEvent {
        crate::event::AgentEvent::Approval {
            id: self.approval_id.clone(),
            question: self.question.clone(),
            detail: self.detail.clone(),
            options: self.options.clone(),
            expires_at_ms: self.expires_at_ms,
        }
    }
}

pub trait Surface: Send + Sync {
    /// One runtime occurrence, sequenced and serde-shaped. Implementations
    /// pick the [`crate::event::AgentEvent`] variants they render.
    fn emit(&self, ev: &crate::event::Envelope);
}
