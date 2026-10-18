//! The operator status report: one struct every surface renders the same
//! way (`/status`, `ka-agent status`, the web status page, attach hello).

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct StatusReport {
    pub session_id: String,
    /// `Debug` of the session status (`Idle`, `AwaitingApproval`, …).
    pub status: String,
    pub strand_id: Option<String>,
    pub model: Option<String>,
    pub mode: String,
    pub worker_running: bool,
    pub idle_timeout_secs: u64,
    pub spent_usd: f64,
    pub spent_tokens: u64,
    pub limit_usd: Option<f64>,
    pub limit_tokens: Option<u64>,
    pub queue: usize,
    pub pending_approvals: usize,
    pub crashes: usize,
    /// The next queued reminder or cron: (title, fire time in ms).
    pub next_task: Option<(String, i64)>,
    /// IANA name of the operator zone.
    pub timezone: String,
}

impl StatusReport {
    /// Five lines, operator wording; times in `now`'s zone.
    pub fn render(&self, now: &jiff::Zoned) -> String {
        let state = match self.status.as_str() {
            "Paused" => "⏸ paused — /resume to continue (budget or provider key)".to_string(),
            "Degraded" => {
                "⚠ degraded — the worker keeps crashing; /new starts a fresh session".to_string()
            }
            other => format!("● {}", snake(other)),
        };
        let model = match &self.model {
            Some(m) => m.clone(),
            None => "model: none — run `ka-agent setup`".to_string(),
        };
        let worker = if self.worker_running {
            if self.idle_timeout_secs == 0 {
                "worker: running (kept alive)".to_string()
            } else {
                format!(
                    "worker: running (stops after {} idle)",
                    ka_agent_core::fmt_interval(self.idle_timeout_secs as i64 * 1000)
                )
            }
        } else {
            "worker: stopped (starts with your next message)".to_string()
        };
        let usd = match self.limit_usd {
            Some(cap) => format!("${:.4} of ${cap:.2}", self.spent_usd),
            None => format!("${:.4} (no limit)", self.spent_usd),
        };
        let tokens = match self.limit_tokens {
            Some(cap) => format!(
                "{} of {} tokens",
                thousands(self.spent_tokens),
                thousands(cap)
            ),
            None => format!("{} tokens", thousands(self.spent_tokens)),
        };
        let mut counters = format!(
            "queue {} · approvals {} · crashes {}",
            self.queue, self.pending_approvals, self.crashes
        );
        if let Some((title, at)) = &self.next_task {
            counters.push_str(&format!(
                " · next: ⏰ {title} — {}",
                ka_agent_core::when::describe(*at, now)
            ));
        }
        let strand = self
            .strand_id
            .as_deref()
            .map(ka_agent_core::ids::short_id)
            .unwrap_or("(new)");
        format!(
            "{state} · {model} · mode {}\n{worker}\ntoday: {usd} · {tokens}\n{counters}\nsession {} · strand {strand} · {}",
            self.mode,
            ka_agent_core::ids::short_id(&self.session_id),
            self.timezone
        )
    }
}

/// `AwaitingApproval` → `awaiting_approval`.
fn snake(s: &str) -> String {
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if c.is_ascii_uppercase() {
            if i > 0 {
                out.push('_');
            }
            out.push(c.to_ascii_lowercase());
        } else {
            out.push(c);
        }
    }
    out
}

/// `12345` → `12,345`.
fn thousands(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    fn report() -> StatusReport {
        StatusReport {
            session_id: "sess_19a3f2b4c1d-00-a1b2c3d4".into(),
            status: "Idle".into(),
            strand_id: None,
            model: Some("anthropic/claude-sonnet-5".into()),
            mode: "guarded".into(),
            worker_running: true,
            idle_timeout_secs: 900,
            spent_usd: 0.0123,
            spent_tokens: 12_345,
            limit_usd: Some(5.0),
            limit_tokens: None,
            queue: 0,
            pending_approvals: 0,
            crashes: 0,
            next_task: None,
            timezone: "Europe/Madrid".into(),
        }
    }

    fn now() -> jiff::Zoned {
        "2026-10-30T10:00:00+01:00[Europe/Madrid]".parse().unwrap()
    }

    #[test]
    fn renders_the_five_operator_lines() {
        let text = report().render(&now());
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(
            lines,
            vec![
                "● idle · anthropic/claude-sonnet-5 · mode guarded",
                "worker: running (stops after 15m idle)",
                "today: $0.0123 of $5.00 · 12,345 tokens",
                "queue 0 · approvals 0 · crashes 0",
                "session a1b2c3d4 · strand (new) · Europe/Madrid",
            ]
        );
    }

    #[test]
    fn variants_name_their_remedy() {
        let mut r = report();
        r.status = "Paused".into();
        r.model = None;
        r.worker_running = false;
        r.limit_usd = None;
        let text = r.render(&now());
        assert!(text.starts_with("⏸ paused — /resume"), "{text}");
        assert!(
            text.contains("model: none — run `ka-agent setup`"),
            "{text}"
        );
        assert!(text.contains("worker: stopped (starts with your next message)"));
        assert!(text.contains("today: $0.0123 (no limit)"));
        r.status = "AwaitingApproval".into();
        assert!(r.render(&now()).starts_with("● awaiting_approval"));
    }

    #[test]
    fn next_task_is_described_in_local_time() {
        let mut r = report();
        let at = now().timestamp().as_millisecond() + 2 * 3_600_000;
        r.next_task = Some(("call mom".into(), at));
        let text = r.render(&now());
        assert!(text.contains("next: ⏰ call mom — today 12:00"), "{text}");
    }
}
