//! One shared rate limiter for the workspace channels (Slack and Discord).
//!
//! Both platforms answer 429 with a header naming when the next call may go
//! out — Slack `Retry-After` (seconds), Discord `Retry-After` (seconds,
//! fractional) and `X-RateLimit-Reset` (epoch seconds) with
//! `X-RateLimit-Remaining`. The limiter keeps a per-key deadline and makes
//! the caller wait for it, so a burst of messages to one channel respects the
//! server's own pacing instead of hammering it.

use std::collections::HashMap;
use std::time::Duration;

use std::sync::Mutex;

/// Per-key pacing state.
#[derive(Debug, Default)]
pub struct RateLimiter {
    /// When each key may be used again (unix ms).
    until: Mutex<HashMap<String, i64>>,
}

impl RateLimiter {
    pub fn new() -> Self {
        Self::default()
    }

    /// The delay still owed by `key` (zero when it may go now).
    pub fn delay_ms(&self, key: &str, now_ms: i64) -> i64 {
        let map = self.until.lock().unwrap_or_else(|e| e.into_inner());
        map.get(key)
            .map(|until| (*until - now_ms).max(0))
            .unwrap_or(0)
    }

    /// Make `key` wait until `until_ms`.
    pub fn defer_until(&self, key: &str, until_ms: i64) {
        let mut map = self.until.lock().unwrap_or_else(|e| e.into_inner());
        let entry = map.entry(key.to_string()).or_insert(until_ms);
        if *entry < until_ms {
            *entry = until_ms;
        }
    }

    /// Wait for `key`'s deadline (sleeps off the async executor only when
    /// there is something to wait for).
    pub async fn wait(&self, key: &str) {
        let now = ka_agent_core::now_ms();
        let delay = self.delay_ms(key, now);
        if delay > 0 {
            tokio::time::sleep(Duration::from_millis(delay as u64)).await;
        }
    }

    /// Parse a 429/limited response's headers into a deadline for `key`.
    /// `headers` are lowercase names; returns the applied deadline.
    pub fn apply_headers(
        &self,
        key: &str,
        headers: &[(&str, String)],
        now_ms: i64,
    ) -> Option<i64> {
        let get = |name: &str| {
            headers
                .iter()
                .find(|(k, _)| *k == name)
                .map(|(_, v)| v.clone())
        };
        let mut until = None;
        // Slack: Retry-After in whole seconds.
        if let Some(v) = get("retry-after")
            && let Ok(secs) = v.trim().parse::<f64>() {
                until = Some(now_ms + (secs.max(0.0) * 1000.0) as i64);
            }
        // Discord: X-RateLimit-Reset is an epoch second; Remaining 0 means
        // this bucket is exhausted.
        if let Some(reset) = get("x-ratelimit-reset")
            && let Ok(epoch) = reset.trim().parse::<f64>() {
                let reset_ms = (epoch * 1000.0) as i64;
                // Only an exhausted bucket defers: a bucket with capacity
                // left answers immediately even though its window resets
                // later.
                let exhausted = get("x-ratelimit-remaining")
                    .and_then(|v| v.trim().parse::<i64>().ok())
                    .is_some_and(|r| r <= 0);
                if exhausted {
                    until = Some(until.map_or(reset_ms, |u: i64| u.max(reset_ms)));
                }
            }
        if let Some(until) = until {
            self.defer_until(key, until);
        }
        until
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[test]
    fn slack_retry_after_defers_the_key() {
        let limiter = RateLimiter::new();
        let now = 1_000_000;
        let until = limiter.apply_headers("chat:C1", &[("retry-after", "2".into())], now);
        assert_eq!(until, Some(now + 2_000));
        assert_eq!(limiter.delay_ms("chat:C1", now), 2_000);
        // Other keys are unaffected.
        assert_eq!(limiter.delay_ms("chat:C2", now), 0);
        // The delay shrinks with time and never goes negative.
        assert_eq!(limiter.delay_ms("chat:C1", now + 1_500), 500);
        assert_eq!(limiter.delay_ms("chat:C1", now + 9_000), 0);
    }

    #[test]
    fn discord_reset_and_remaining() {
        let limiter = RateLimiter::new();
        let now = 1_700_000_000_000;
        let reset_epoch = (now + 4_000) as f64 / 1000.0;
        let until = limiter.apply_headers(
            "guild:1",
            &[
                ("x-ratelimit-reset", format!("{reset_epoch}")),
                ("x-ratelimit-remaining", "0".into()),
            ],
            now,
        );
        assert_eq!(until, Some(now + 4_000));
        // A bucket with capacity left does not defer even though the window
        // resets later.
        let until = limiter.apply_headers(
            "guild:2",
            &[
                ("x-ratelimit-reset", format!("{reset_epoch}")),
                ("x-ratelimit-remaining", "5".into()),
            ],
            now,
        );
        assert_eq!(until, None);
        assert_eq!(limiter.delay_ms("guild:2", now), 0);
    }

    #[test]
    fn fractional_and_malformed_headers() {
        let limiter = RateLimiter::new();
        let now = 10_000;
        assert_eq!(
            limiter.apply_headers("k", &[("retry-after", "0.5".into())], now),
            Some(now + 500)
        );
        assert_eq!(
            limiter.apply_headers("k", &[("retry-after", "soon".into())], now),
            None
        );
    }
}
