//! The Telegram channel (SEED.md §5): Bot API long polling in, throttled
//! messages + approval keyboards out. Final answers and notices render a
//! safe HTML subset (`ka_agent_core::markdown`, everything escaped first) with a
//! plain-text fallback; router replies and approval cards stay plain.

//! Split from telegram.rs (moves only).

pub(crate) mod channel;
pub(crate) mod client;
pub(crate) mod media;
pub(crate) mod surface;

pub use channel::{TelegramChannel, TelegramConfig};
pub(crate) use client::TelegramClient;
pub use client::telegram_get_me;
pub use surface::TelegramSurface;

use std::time::Duration;

use thiserror::Error;
const POLL_TIMEOUT_SECS: u64 = 25;
const POLL_HTTP_TIMEOUT: Duration = Duration::from_secs(40);
const TYPING_INTERVAL: Duration = Duration::from_secs(4);
const NOTE_EDIT_INTERVAL_MS: i64 = 3_000;
const SEND_SPACING_MS: i64 = 1_000;
const UNAUTHORIZED_NOTE_SECS: i64 = 60;
const START_REPLY_INTERVAL_MS: i64 = 60 * 60 * 1000;
const MAX_MESSAGE_CHARS: usize = 4096;
const START_REPLY: &str = "This is a private assistant. If you are its owner, run `ka-agent pair telegram` on your computer and send the code here.";

#[derive(Debug, Error)]
pub(crate) enum ChannelError {
    #[error("telegram api: {description}")]
    Api { description: String },
    #[error("telegram http: {0}")]
    Http(#[from] reqwest::Error),
}

impl ChannelError {
    fn api(description: impl Into<String>) -> Self {
        Self::Api {
            description: description.into(),
        }
    }

    /// Bad-token failures: Telegram answers 401/404 with these descriptions.
    pub(crate) fn is_auth_failure(&self) -> bool {
        match self {
            Self::Api { description } => {
                description.contains("Unauthorized")
                    || description.contains("Not Found")
                    || description.contains("401")
                    || description.contains("404")
            }
            Self::Http(_) => false,
        }
    }
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Count of ``` fences in `text` is odd → make the chunk render as a
/// balanced block: if the chunk itself opens the block (starts with a
/// fence) append a closer; if it continues a block opened in an earlier
/// chunk, prepend the opener.
pub(crate) fn balance_fences(text: &str) -> String {
    if text.matches("```").count().is_multiple_of(2) {
        return text.to_string();
    }
    if text.trim_start().starts_with("```") {
        format!("{text}\n```")
    } else {
        format!("```\n{text}")
    }
}

/// Last `n` characters of an id, for compact rendering and callback data.
pub(crate) fn short_id(id: &str, n: usize) -> String {
    id.chars()
        .rev()
        .take(n)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::balance_fences;

    #[test]
    fn fence_balancing() {
        // even count: untouched
        assert_eq!(balance_fences("plain"), "plain");
        assert_eq!(balance_fences("```a\nb``` tail"), "```a\nb``` tail");
        // odd count, block opened in this chunk: closer appended
        assert_eq!(
            balance_fences("```rust\nfn main() {}"),
            "```rust\nfn main() {}\n```"
        );
        // odd count, chunk continues an earlier block: opener prepended
        assert_eq!(
            balance_fences("still in the block```"),
            "```\nstill in the block```"
        );
        // every balanced result has an even fence count
        for balanced in [
            balance_fences("```rust\nfn main() {}"),
            balance_fences("still in the block```"),
        ] {
            assert_eq!(balanced.matches("```").count() % 2, 0);
        }
    }
}
