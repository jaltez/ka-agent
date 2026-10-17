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

/// Truncate a string to at most `cap` **bytes** without splitting a UTF-8
/// character: a plain `String::truncate`/slice panics when the cut lands
/// mid-character, and every caller here cuts model- or network-generated
/// text where that is the common case, not the edge.
pub fn truncate_at_char(s: &str, cap: usize) -> &str {
    if s.len() <= cap {
        return s;
    }
    let mut cut = cap;
    while cut > 0 && !s.is_char_boundary(cut) {
        cut -= 1;
    }
    &s[..cut]
}

#[cfg(test)]
mod truncate_tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::truncate_at_char;

    #[test]
    fn ascii_cuts_at_the_cap() {
        assert_eq!(truncate_at_char("abcdef", 3), "abc");
        assert_eq!(truncate_at_char("abc", 3), "abc");
        assert_eq!(truncate_at_char("", 3), "");
    }

    #[test]
    fn multibyte_never_splits_a_character() {
        // "ñ" is 2 bytes; the cut at byte 3 lands inside the second "ñ".
        let s = "año año año";
        let cut = truncate_at_char(s, 3);
        assert_eq!(cut, "añ", "byte 3 is a boundary: ñ ends there");
        // Byte 2 would split ñ → falls back to the boundary before it.
        assert_eq!(truncate_at_char(s, 2), "a");
        // Every cut point in a CJK-heavy string yields valid text.
        let cjk = "日本語日本語日本語";
        for cap in 0..=cjk.len() {
            let cut = truncate_at_char(cjk, cap);
            assert!(cut.chars().all(|c| !c.is_ascii()), "cap {cap}: {cut:?}");
        }
        // Emoji (4 bytes) at the boundary.
        assert_eq!(truncate_at_char("a🦀🦀🦀", 2), "a");
        assert_eq!(truncate_at_char("a🦀🦀🦀", 5), "a🦀");
    }

    #[test]
    fn cap_zero_yields_empty() {
        assert_eq!(truncate_at_char("abc", 0), "");
    }
}
