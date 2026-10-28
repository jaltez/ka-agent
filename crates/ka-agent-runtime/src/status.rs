//! The operator status report: one struct every surface renders the same
//! way (`/status`, `ka-agent status`, the web status page, attach hello).

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct StatusReport {
    pub session_id: String,
    /// The assistant's persona name (`Ka` by default).
    #[serde(default)]
    pub persona: String,
    /// When this runtime woke up (ms since epoch), for the header.
    #[serde(default)]
    pub awake_since_ms: i64,
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
    /// What the engine can reach beyond the workspace: web search
    /// provider, operator MCP servers, fallback models.
    #[serde(default)]
    pub capabilities: Capabilities,
    /// Compiled memory against its budget, and proposals awaiting the
    /// operator.
    #[serde(default)]
    pub memory: Option<MemoryUse>,
}

/// How full the compiled memory is (it truncates silently past the budget).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct MemoryUse {
    pub used_bytes: usize,
    pub budget_bytes: usize,
    pub truncated: bool,
    pub to_review: usize,
}

impl MemoryUse {
    /// `memory 4.1 of 6.0 KB · 2 to review` (+ a warning when it no longer
    /// fits).
    pub fn line(&self) -> String {
        let kb = |b: usize| b as f64 / 1024.0;
        let mut out = format!(
            "memory {:.1} of {:.1} KB",
            kb(self.used_bytes),
            kb(self.budget_bytes)
        );
        if self.to_review > 0 {
            out.push_str(&format!(" · {} to review (/memory review)", self.to_review));
        }
        if self.truncated {
            out.push_str(" · ⚠ full — older entries are left out; /memory to prune");
        } else if self.used_bytes * 10 >= self.budget_bytes * 9 {
            out.push_str(" · nearly full");
        }
        out
    }
}

/// The configured capability set, for one status line.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Capabilities {
    pub search: Option<String>,
    #[serde(default)]
    pub voice: Option<String>,
    pub mcp: Vec<String>,
    pub fallback: Vec<String>,
}

impl Capabilities {
    pub fn from_config(cfg: &ka_agent_core::DaemonConfig) -> Self {
        Self {
            search: cfg.search.as_ref().map(|s| s.provider.clone()),
            voice: cfg.voice.as_ref().map(|v| v.provider.clone()),
            mcp: cfg.mcp.iter().map(|m| m.name.clone()).collect(),
            fallback: cfg.fallback.clone(),
        }
    }

    /// `tools: web search (tavily) · mcp github, home · fallback openai/gpt-5.1`,
    /// or `None` when nothing is configured.
    fn line(&self) -> Option<String> {
        let mut parts = Vec::new();
        if let Some(p) = &self.search {
            parts.push(format!("web search ({p})"));
        }
        if let Some(p) = &self.voice {
            parts.push(format!("voice notes ({p})"));
        }
        if !self.mcp.is_empty() {
            parts.push(format!("mcp {}", self.mcp.join(", ")));
        }
        if !self.fallback.is_empty() {
            parts.push(format!("fallback {}", self.fallback.join(" → ")));
        }
        (!parts.is_empty()).then(|| format!("tools: {}", parts.join(" · ")))
    }
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
        // Anything looser than `ask` is worth a glance every time.
        let since = jiff::Timestamp::from_millisecond(self.awake_since_ms)
            .map(|t| t.to_zoned(now.time_zone().clone()))
            .map(|z| format!("{:02}:{:02}", z.hour(), z.minute()))
            .unwrap_or_else(|_| "--:--".to_string());
        let persona = if self.persona.is_empty() {
            "Ka".to_string()
        } else {
            self.persona.clone()
        };
        let mode = match self.mode.as_str() {
            "allow" => "⚠ mode allow (acts without asking)".to_string(),
            "accept_edits" => "mode accept_edits (edits without asking)".to_string(),
            "readonly" => "mode readonly (read and plan only, never executes)".to_string(),
            m => format!("mode {m}"),
        };
        let mut out = format!(
            "☥ {persona} · awake since {since} · {state} · {model} · {mode}\n{worker}\ntoday: {usd} · {tokens}\n{counters}\nsession {} · strand {strand} · {}",
            ka_agent_core::ids::short_id(&self.session_id),
            self.timezone
        );
        if let Some(m) = &self.memory {
            out.push('\n');
            out.push_str(&m.line());
        }
        if let Some(line) = self.capabilities.line() {
            out.push('\n');
            out.push_str(&line);
        }
        out
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
            persona: "Ka".into(),
            awake_since_ms: 0,
            session_id: "sess_19a3f2b4c1d-00-a1b2c3d4".into(),
            status: "Idle".into(),
            strand_id: None,
            model: Some("anthropic/claude-sonnet-5".into()),
            mode: "ask".into(),
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
            capabilities: Capabilities::default(),
            memory: None,
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
                "☥ Ka · awake since 01:00 · ● idle · anthropic/claude-sonnet-5 · mode ask",
                "worker: running (stops after 15m idle)",
                "today: $0.0123 of $5.00 · 12,345 tokens",
                "queue 0 · approvals 0 · crashes 0",
                "session a1b2c3d4 · strand (new) · Europe/Madrid",
            ]
        );
    }

    #[test]
    fn memory_line_warns_before_it_silently_truncates() {
        let mut r = report();
        r.memory = Some(MemoryUse {
            used_bytes: 4200,
            budget_bytes: 6144,
            truncated: false,
            to_review: 2,
        });
        let text = r.render(&now());
        assert!(
            text.contains("memory 4.1 of 6.0 KB · 2 to review (/memory review)"),
            "{text}"
        );
        let near = MemoryUse {
            used_bytes: 5600,
            budget_bytes: 6144,
            ..MemoryUse::default()
        };
        assert!(near.line().ends_with("nearly full"), "{}", near.line());
        let full = MemoryUse {
            used_bytes: 6144,
            budget_bytes: 6144,
            truncated: true,
            to_review: 0,
        };
        assert!(full.line().contains("⚠ full"), "{}", full.line());
    }

    #[test]
    fn capabilities_add_one_line_only_when_configured() {
        let mut r = report();
        r.capabilities = Capabilities {
            search: Some("tavily".into()),
            voice: None,
            mcp: vec!["github".into(), "home".into()],
            fallback: vec!["openai/gpt-5.1".into()],
        };
        let text = r.render(&now());
        assert_eq!(
            text.lines().last(),
            Some("tools: web search (tavily) · mcp github, home · fallback openai/gpt-5.1")
        );
        assert_eq!(report().render(&now()).lines().count(), 5);
    }

    #[test]
    fn variants_name_their_remedy() {
        let mut r = report();
        r.status = "Paused".into();
        r.model = None;
        r.worker_running = false;
        r.limit_usd = None;
        let text = r.render(&now());
        assert!(text.contains("⏸ paused — /resume"), "{text}");
        assert!(
            text.contains("model: none — run `ka-agent setup`"),
            "{text}"
        );
        assert!(text.contains("worker: stopped (starts with your next message)"));
        assert!(text.contains("today: $0.0123 (no limit)"));
        r.status = "AwaitingApproval".into();
        assert!(r.render(&now()).contains("● awaiting_approval"));
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
