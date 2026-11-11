//! Structured error cards: one friendly shape per error class, shared by
//! every channel surface. Raw provider messages never reach the operator
//! unframed — the first 300 chars ride along as detail.

/// The rendered pieces of one error: a headline (emoji + title), an
/// optional explanation line, a remedy hint, and the bounded raw detail.
pub(crate) struct FriendlyError {
    pub emoji: &'static str,
    pub title: String,
    pub line: &'static str,
    pub hint: &'static str,
    /// First 300 chars of the raw message; [`FriendlyError::html`]
    /// escapes it, the plain form shows it verbatim.
    pub detail: String,
}

/// The raw-detail cap: enough to recognize the failure, never a stack
/// trace. The whole card stays far under Telegram's 4096-char ceiling by
/// construction (title + line + hint + 300 < 500).
const DETAIL_CHARS: usize = 300;

/// Escape `& < >` for Telegram's HTML parse mode (the same escape
/// [`ka_agent_core::markdown`] applies to model text).
fn html_escape(s: &str) -> String {
    ka_agent_core::markdown::escape(s, ka_agent_core::markdown::Target::Telegram)
}

/// Map one engine error class onto its operator-facing card.
pub(crate) fn friendly_error(class: &str, message: &str) -> FriendlyError {
    let detail: String = message.chars().take(DETAIL_CHARS).collect();
    match class {
        "rate_limit" => FriendlyError {
            emoji: "⏳",
            title: "Rate limited".into(),
            line: "the provider is throttling requests",
            hint: "tap ↻ Retry in a minute",
            detail,
        },
        "network" => FriendlyError {
            emoji: "📡",
            title: "Can't reach the provider".into(),
            line: "connection trouble reaching the model",
            hint: "↻ Retry once you're back online",
            detail,
        },
        "auth" => FriendlyError {
            emoji: "🔑",
            title: "Provider rejected the key".into(),
            line: "the API key was refused",
            hint: "fix the key (ka-agent key set …), then /resume",
            detail,
        },
        "overflow" => FriendlyError {
            emoji: "📄",
            title: "Conversation too large".into(),
            line: "the session exceeds the model's context",
            hint: "/new starts a fresh session",
            detail,
        },
        "protocol" => FriendlyError {
            emoji: "🔌",
            title: "Engine protocol error".into(),
            line: "",
            hint: "see /status",
            detail,
        },
        "unsupported" => FriendlyError {
            emoji: "🚫",
            title: "Unsupported request".into(),
            line: "",
            hint: "",
            detail,
        },
        "internal" => FriendlyError {
            emoji: "⚙️",
            title: "Internal error".into(),
            line: "",
            hint: "see /status",
            detail,
        },
        "fatal" => FriendlyError {
            emoji: "⚙️",
            title: "Worker crashed".into(),
            line: "",
            hint: "the worker restarts automatically",
            detail,
        },
        other => FriendlyError {
            emoji: "⚠️",
            title: format!("Error [{other}]"),
            line: "",
            hint: "",
            detail,
        },
    }
}

impl FriendlyError {
    /// The plain-text form: `{emoji} {title}` plus the non-empty lines.
    pub fn plain(&self) -> String {
        let mut out = format!("{} {}", self.emoji, self.title);
        for part in [&self.line, &self.hint] {
            if !part.is_empty() {
                out.push('\n');
                out.push_str(part);
            }
        }
        if !self.detail.is_empty() {
            out.push('\n');
            out.push_str(&self.detail);
        }
        out
    }

    /// The HTML form for Telegram's `parse_mode: HTML`: a bold headline,
    /// the non-empty lines, and the detail as one code block.
    pub fn html(&self) -> String {
        let mut out = format!("<b>{} {}</b>", self.emoji, html_escape(&self.title));
        for part in [&self.line, &self.hint] {
            if !part.is_empty() {
                out.push('\n');
                // static operator chrome, already HTML-free
                out.push_str(part);
            }
        }
        if !self.detail.is_empty() {
            out.push_str("\n<code>");
            out.push_str(&html_escape(&self.detail));
            out.push_str("</code>");
        }
        out
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    #[test]
    fn known_classes_map_and_escape() {
        let f = friendly_error("rate_limit", "slow down <batch> & retry");
        assert_eq!(f.emoji, "⏳");
        assert_eq!(f.title, "Rate limited");
        assert_eq!(f.detail, "slow down <batch> & retry");
        assert!(f.html().contains("<b>⏳ Rate limited</b>"));
        assert!(f.html().contains("&lt;batch&gt;"));
        assert!(f.html().contains("<code>"));
        assert!(f.plain().starts_with("⏳ Rate limited\n"));
        assert!(f.plain().contains("<batch>"));
    }

    #[test]
    fn unknown_class_names_itself() {
        let f = friendly_error("weird", "boom");
        assert_eq!(f.title, "Error [weird]");
        assert_eq!(f.emoji, "⚠️");
    }

    #[test]
    fn detail_is_capped_on_a_char_boundary() {
        let long: String = "é".repeat(400);
        let f = friendly_error("internal", &long);
        assert_eq!(f.detail.chars().count(), DETAIL_CHARS);
    }

    #[test]
    fn cards_stay_under_the_message_ceiling() {
        for class in [
            "rate_limit",
            "network",
            "auth",
            "overflow",
            "protocol",
            "unsupported",
            "internal",
            "fatal",
            "unknown-thing",
        ] {
            let long = "x".repeat(10_000);
            let f = friendly_error(class, &long);
            assert!(f.plain().chars().count() < 500, "{class}");
            assert!(f.html().chars().count() < 500, "{class}");
        }
    }
}
