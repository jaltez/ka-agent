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

use serde_json::Value;
use thiserror::Error;
const POLL_TIMEOUT_SECS: u64 = 25;
const POLL_HTTP_TIMEOUT: Duration = Duration::from_secs(40);
/// Media uploads (`sendPhoto`/`sendDocument`) get their own timeout: the
/// tool cap is 50 MB, which does not fit 40 s over a slow upstream link.
const MEDIA_HTTP_TIMEOUT: Duration = Duration::from_secs(10 * 60);
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
    Api {
        description: String,
        /// Bot API `error_code`: 429 is flood control (retry), 400/404 mean
        /// the method itself is unavailable.
        error_code: Option<i64>,
        /// `parameters.retry_after` (seconds) on a 429 answer.
        retry_after: Option<i64>,
    },
    #[error("telegram http: {0}")]
    Http(#[from] reqwest::Error),
}

impl ChannelError {
    fn api(description: impl Into<String>) -> Self {
        Self::Api {
            description: description.into(),
            error_code: None,
            retry_after: None,
        }
    }

    /// One Bot API `{ok:false,…}` answer, keeping the fields a caller needs
    /// to tell a transient refusal (429) from a missing method (404).
    fn from_api_answer(v: &Value) -> Self {
        Self::Api {
            description: v["description"]
                .as_str()
                .unwrap_or("unknown api error")
                .to_string(),
            error_code: v["error_code"].as_i64(),
            retry_after: v["parameters"]["retry_after"].as_i64(),
        }
    }

    /// Bad-token failures: Telegram answers 401/404 with these descriptions.
    pub(crate) fn is_auth_failure(&self) -> bool {
        match self {
            Self::Api {
                description,
                error_code,
                ..
            } => {
                matches!(error_code, Some(401) | Some(404))
                    || description.contains("Unauthorized")
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
    use super::{ChannelError, MEDIA_HTTP_TIMEOUT, POLL_HTTP_TIMEOUT};

    #[test]
    fn media_uploads_get_a_longer_timeout() {
        // A 50 MB `sendDocument` does not fit the polling timeout.
        assert_eq!(MEDIA_HTTP_TIMEOUT, std::time::Duration::from_secs(600));
        assert!(MEDIA_HTTP_TIMEOUT > POLL_HTTP_TIMEOUT);
    }

    #[test]
    fn api_errors_keep_the_code_and_retry_after() {
        let err = ChannelError::from_api_answer(&serde_json::json!({
            "ok": false,
            "error_code": 429,
            "description": "Too Many Requests: retry after 7",
            "parameters": { "retry_after": 7 },
        }));
        match err {
            ChannelError::Api {
                error_code,
                retry_after,
                ..
            } => {
                assert_eq!(error_code, Some(429));
                assert_eq!(retry_after, Some(7));
            }
            other => panic!("expected an API error, got {other:?}"),
        }
    }

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
