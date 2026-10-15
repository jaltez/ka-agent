//! The durable inbound queue (SEED.md §3.8, gap G4 mitigation): ka drops
//! prompts that arrive mid-cycle, so Ka Agent queues every message that is
//! not submittable right now and drains one per `idle`.

use serde::{Deserialize, Serialize};

use crate::ids::{QueueId, SessionId};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueueItem {
    pub id: QueueId,
    pub session: SessionId,
    pub text: String,
    pub source: String,
    pub enqueued_ms: i64,
    /// Set when the item became a turn (submitted or interrupted).
    pub turned: Option<crate::ids::TurnId>,
}

impl QueueItem {
    pub fn new(session: SessionId, text: String, source: &str) -> Self {
        Self {
            id: QueueId::generate(),
            session,
            text,
            source: source.to_string(),
            enqueued_ms: crate::now_ms(),
            turned: None,
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    #[test]
    fn queue_item_fields() {
        let q = super::QueueItem::new(
            crate::ids::SessionId::generate(),
            "later".into(),
            "telegram:123",
        );
        assert!(q.id.0.starts_with("q_"));
        assert_eq!(q.source, "telegram:123");
        assert!(q.turned.is_none());
    }
}
