//! Outbound message chunks (SEED.md §3.7): delivery is keyed by
//! `(turn, chunk_index)` so a crash mid-send can neither reorder nor
//! duplicate beyond the single chunk in flight.

use serde::{Deserialize, Serialize};

use crate::ids::TurnId;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OutboxStatus {
    Queued,
    Sent,
    Failed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OutboxChunk {
    pub turn: TurnId,
    pub chunk_index: u32,
    pub text: String,
    pub status: OutboxStatus,
    /// Channel message id once delivered (Telegram `message_id`).
    pub channel_message_id: Option<i64>,
    pub created_ms: i64,
}

impl OutboxChunk {
    pub fn new(turn: TurnId, chunk_index: u32, text: String) -> Self {
        Self {
            turn,
            chunk_index,
            text,
            status: OutboxStatus::Queued,
            channel_message_id: None,
            created_ms: crate::now_ms(),
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    #[test]
    fn chunk_key() {
        let c = super::OutboxChunk::new(crate::ids::TurnId::generate(), 2, "part".into());
        assert_eq!(c.chunk_index, 2);
        assert_eq!(c.status, super::OutboxStatus::Queued);
    }
}
