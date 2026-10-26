//! Rendering for the inline terminal client: permanent output printed
//! above, a live area below ([in-progress line] + [activity] + [status] +
//! [editor rows]) redrawn inside synchronized-output markers.

use super::editor::Editor;
use ka_agent_runtime::AgentEvent;
use unicode_width::UnicodeWidthStr;

/// Everything the live area shows.
pub struct View {
    /// The assistant's partial answer (the last line printed so far is
    /// counted so the live line can be replaced).
    pub partial: String,
    /// `▸ tool detail`, `▸ N tools running`, `∴ thinking…`, `⋯ compacting`.
    pub activity: String,
    pub running: bool,
    pub tool_count: usize,
    pub thought_chars: usize,
    pub saw_answer: bool,
    /// Status-line inputs.
    pub name: String,
    pub model: String,
    pub effort: Option<String>,
    pub context_pct: Option<u8>,
    pub spent_today: f64,
    pub daily_cap: Option<f64>,
    pub queued: usize,
    pub plan: Option<(usize, usize)>,
    pub mode: String,
    pub focused: bool,
    pub verbose: bool,
    pub width: usize,
    spinner: usize,
}

const SPINNER: &[char] = &['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];

impl View {
    pub fn new(name: String, width: usize) -> Self {
        Self {
            partial: String::new(),
            activity: String::new(),
            running: false,
            tool_count: 0,
            thought_chars: 0,
            saw_answer: false,
            name,
            model: String::new(),
            effort: None,
            context_pct: None,
            spent_today: 0.0,
            daily_cap: None,
            queued: 0,
            plan: None,
            mode: "guarded".into(),
            focused: true,
            verbose: false,
            width,
            spinner: 0,
        }
    }

    pub fn tick(&mut self) {
        self.spinner = (self.spinner + 1) % SPINNER.len();
    }

    /// The status line (dim, truncated to width).
    pub fn status_line(&self) -> String {
        let mut parts: Vec<String> = Vec::new();
        let spin = if self.running {
            format!("{} ", SPINNER[self.spinner])
        } else {
            String::new()
        };
        parts.push(format!("{spin}☥ {}", self.name));
        if !self.model.is_empty() {
            parts.push(self.model.clone());
        }
        if let Some(e) = &self.effort {
            parts.push(e.clone());
        }
        if let Some(pct) = self.context_pct {
            parts.push(format!("ctx {pct}%"));
        }
        match (self.spent_today, self.daily_cap) {
            (spent, Some(cap)) => parts.push(format!("${spent:.2}/{cap:.2}")),
            (spent, None) => parts.push(format!("${spent:.2}")),
        }
        if self.queued > 0 {
            parts.push(format!("queued {}", self.queued));
        }
        if let Some((done, total)) = self.plan {
            parts.push(format!("plan {done}/{total}"));
        }
        if self.mode != "guarded" {
            parts.push(format!("mode {}", self.mode));
        }
        let mut line = parts.join(" · ");
        if !self.focused {
            line.push_str(" · unfocused");
        }
        truncate_display(&line, self.width)
    }

    /// The activity line (empty when idle).
    pub fn activity_line(&self) -> String {
        if !self.running {
            return String::new();
        }
        if !self.activity.is_empty() {
            return truncate_display(&self.activity, self.width);
        }
        if !self.saw_answer && self.thought_chars > 0 {
            return format!("∴ thinking… {}", self.thought_chars);
        }
        String::new()
    }

    /// Redraw the live area: move to its first row from wherever the cursor
    /// was (`cursor_up`), clear down, print activity + status + the draft,
    /// and leave the cursor on the editor row. Returns the new `cursor_up`.
    pub fn redraw(&self, editor: &Editor, cursor_up: usize) -> usize {
        let ops = self.redraw_ops(editor, cursor_up);
        super::paint::emit(&ops);
        self.cursor_up(editor, cursor_up)
    }

    /// The redraw as drawing commands (platform-neutral; tested on Unix as
    /// the exact ANSI bytes the console has always printed).
    pub fn redraw_ops(&self, editor: &Editor, cursor_up: usize) -> Vec<super::paint::Op> {
        use super::paint::Op;
        let mut ops = vec![Op::SyncBegin];
        if cursor_up > 0 {
            ops.push(Op::Up(cursor_up));
        }
        ops.push(Op::Cr);
        ops.push(Op::ClearDown);
        let activity = self.activity_line();
        if !activity.is_empty() {
            ops.push(Op::Dim(activity));
            ops.push(Op::Text("\r\n".into()));
        }
        // The status line is always present (spinner only while running).
        ops.push(Op::Dim(self.status_line()));
        ops.push(Op::Text("\r\n".into()));
        // The draft, soft-wrapped; the last row holds the cursor.
        let (rows, (crow, ccol)) = editor.layout(self.width);
        for (i, (s, e)) in rows.iter().enumerate() {
            let text: String = editor_text_range(editor, *s, *e);
            if i == rows.len() - 1 {
                ops.push(Op::Bold("»".into()));
                ops.push(Op::Text(format!(" {text}")));
            } else {
                ops.push(Op::Text(format!("  {text}\r\n")));
            }
        }
        // From the last printed row up to the cursor's row (they differ only
        // when the draft wraps).
        let up = rows.len().saturating_sub(1 + crow);
        if up > 0 {
            ops.push(Op::Up(up));
        }
        ops.push(Op::Cr);
        ops.push(Op::Right(2 + ccol));
        ops.push(Op::SyncEnd);
        ops
    }

    /// Rows between the live area's first row and the cursor: the editor
    /// rows above the cursor's row plus the activity and status rows, so
    /// the next redraw starts at the top.
    pub fn cursor_up(&self, editor: &Editor, _cursor_up: usize) -> usize {
        let (rows, (crow, _)) = editor.layout(self.width);
        let above = rows.len().saturating_sub(1 + crow);
        above + usize::from(!self.activity_line().is_empty()) + 1
    }
}

/// The text of one layout row (public so the view and tests agree).
fn editor_text_range(editor: &Editor, start: usize, end: usize) -> String {
    // The editor's chars are private; go through the text once.
    editor
        .text()
        .chars()
        .skip(start)
        .take(end - start)
        .collect()
}

/// Truncate to `width` display columns.
pub fn truncate_display(s: &str, width: usize) -> String {
    if s.width() <= width {
        return s.to_string();
    }
    let mut out = String::new();
    let mut used = 0usize;
    for c in s.chars() {
        let w = unicode_width::UnicodeWidthChar::width(c).unwrap_or(0);
        if used + w + 1 > width {
            out.push('…');
            break;
        }
        out.push(c);
        used += w;
    }
    out
}

/// A one-line human label for a tool event (pure; unit-tested).
pub fn tool_line(tool: &str, detail: &str) -> String {
    let detail = truncate_display(detail, 60);
    if detail.is_empty() {
        format!("▸ {}", ka_agent_core::labels::tool_label(tool, ""))
    } else {
        format!(
            "▸ {} {detail}",
            ka_agent_core::labels::tool_label(tool, &detail)
        )
    }
}

/// Permanent line for a finished tool.
pub fn tool_done_line(tool: &str, ms: u64) -> String {
    format!("  ✓ {tool} {ms}ms")
}

/// Permanent line for a failed tool.
pub fn tool_failed_line(tool: &str, error: &str) -> String {
    let first = error.lines().next().unwrap_or("");
    format!("  ✗ {tool} · {}", truncate_display(first, 60))
}

/// Feed one event into the view state; returns the permanent lines to print
/// (the caller prints them above the live area).
/// Take the streamed answer and render it as permanent Markdown rows, with
/// the same line renderer the piped path uses.
fn render_markdown_take(partial: &mut String) -> Vec<String> {
    use ka_agent_core::markdown::{
        Target, fence_lang, render_code_line, render_fence_open, render_line,
    };
    let text = std::mem::take(partial);
    if text.trim().is_empty() {
        return Vec::new();
    }
    let target = Target::Ansi;
    let mut out = Vec::new();
    let mut in_fence = false;
    for line in text.trim_end().lines() {
        if in_fence {
            if line.trim() == "```" {
                in_fence = false;
                out.push("\x1b[2m──\x1b[22m".to_string());
            } else {
                out.push(render_code_line(line, target));
            }
        } else if let Some(rest) = line.trim_start().strip_prefix("```") {
            in_fence = true;
            out.push(render_fence_open(fence_lang(rest)));
        } else {
            out.push(render_line(line, target));
        }
    }
    out.push(String::new());
    out
}

pub fn absorb(view: &mut View, ev: &AgentEvent, verbose: bool) -> Vec<String> {
    match ev {
        AgentEvent::TurnStarted { .. } => {
            view.running = true;
            view.saw_answer = false;
            view.thought_chars = 0;
            view.tool_count = 0;
            view.activity.clear();
            view.partial.clear();
        }
        AgentEvent::TurnFinished {
            stop,
            cost_usd,
            tokens,
            ms,
            ..
        } => {
            view.running = false;
            view.activity.clear();
            // The streamed draft becomes a permanent block, then the footer.
            let mut out = render_markdown_take(&mut view.partial);
            out.push(format!(
                "\x1b[2m· {:.1}s · ${cost_usd:.4} · {} tok{}\x1b[0m",
                *ms as f64 / 1000.0,
                tokens,
                if stop == "done" {
                    String::new()
                } else {
                    format!(" · {stop}")
                }
            ));
            return out;
        }
        AgentEvent::Delta { text } => {
            view.saw_answer = true;
            view.activity.clear();
            view.partial.push_str(text);
        }
        AgentEvent::Thought { text } => {
            view.thought_chars += text.chars().count();
            if verbose {
                return vec![format!("\x1b[2m{}\x1b[0m", text.trim_end())];
            }
        }
        AgentEvent::ToolStarted { tool, detail, .. } => {
            view.tool_count += 1;
            view.saw_answer = false;
            view.activity = if view.tool_count > 1 {
                format!("▸ {} tools running", view.tool_count)
            } else {
                tool_line(tool, detail)
            };
        }
        AgentEvent::ToolFinished {
            tool,
            ok,
            ms,
            error,
            ..
        } => {
            view.tool_count = view.tool_count.saturating_sub(1);
            view.activity = if view.tool_count > 0 {
                format!("▸ {} tools running", view.tool_count)
            } else {
                String::new()
            };
            return vec![if *ok {
                tool_done_line(tool, *ms)
            } else {
                tool_failed_line(tool, error.as_deref().unwrap_or("failed"))
            }];
        }
        AgentEvent::Plan { items } => {
            let done = items.iter().filter(|i| i.state == "done").count();
            view.plan = Some((done, items.len()));
        }
        AgentEvent::Context { used, window } => {
            if *window > 0 {
                view.context_pct = Some(((*used as f64 / *window as f64) * 100.0) as u8);
            }
        }
        AgentEvent::Compacting { done } => {
            if *done {
                view.activity.clear();
            } else {
                view.activity = "⋯ compacting context".into();
            }
        }
        AgentEvent::Queue { depth } => view.queued = *depth as usize,
        AgentEvent::Settings {
            model,
            mode,
            effort,
        } => {
            if let Some(m) = model {
                view.model = m.clone();
            }
            if let Some(m) = mode {
                view.mode = m.clone();
            }
            view.effort = effort.clone();
        }
        AgentEvent::File { path, .. } => return vec![format!("📎 {path}")],
        AgentEvent::Reminder { text, .. } => return vec![format!("⏰ {text}")],
        AgentEvent::Error { class, message } => {
            return vec![format!("\x1b[31m! [{class}] {message}\x1b[0m")];
        }
        _ => {}
    }
    Vec::new()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;
    use ka_agent_runtime::AgentEvent;

    #[test]
    fn status_line_truncates_and_shows_plan_and_mode() {
        let mut v = View::new("Ka".into(), 120);
        v.model = "zai/glm-5.3".into();
        v.context_pct = Some(42);
        v.spent_today = 0.4;
        v.daily_cap = Some(3.0);
        v.queued = 2;
        v.plan = Some((1, 3));
        v.mode = "plan".into();
        let line = v.status_line();
        assert!(line.contains("☥ Ka"), "{line}");
        assert!(line.contains("ctx 42%"), "{line}");
        assert!(line.contains("$0.40/3.00"), "{line}");
        assert!(line.contains("queued 2"), "{line}");
        assert!(line.contains("plan 1/3"), "{line}");
        assert!(line.contains("mode plan"), "{line}");

        v.width = 20;
        let short = v.status_line();
        assert!(short.width() <= 20, "{short}");
        assert!(short.ends_with('…'), "{short}");
    }

    #[test]
    fn activity_steps_through_tools_and_thinking() {
        let mut v = View::new("Ka".into(), 80);
        let started = |tool: &str, detail: &str| AgentEvent::ToolStarted {
            id: "c1".into(),
            tool: tool.into(),
            detail: detail.into(),
        };
        absorb(
            &mut v,
            &AgentEvent::TurnStarted {
                turn: "t".into(),
                source: "console".into(),
            },
            false,
        );
        absorb(&mut v, &started("bash", "cargo test"), false);
        assert!(
            v.activity_line().contains("running a command"),
            "{}",
            v.activity_line()
        );
        absorb(&mut v, &started("read", "a.rs"), false);
        assert!(
            v.activity_line().contains("2 tools running"),
            "{}",
            v.activity_line()
        );
        let finished = |tool: &str| AgentEvent::ToolFinished {
            id: "c1".into(),
            tool: tool.into(),
            ok: true,
            ms: 12,
            error: None,
        };
        let done = absorb(&mut v, &finished("bash"), false);
        assert!(done[0].contains("✓ bash 12ms"), "{:?}", done);
        absorb(&mut v, &finished("read"), false);
        assert!(v.activity_line().is_empty(), "{}", v.activity_line());
        // Thinking shows until answer text arrives.
        absorb(
            &mut v,
            &AgentEvent::Thought {
                text: "pondering".into(),
            },
            false,
        );
        assert!(
            v.activity_line().contains("thinking…"),
            "{}",
            v.activity_line()
        );
        absorb(
            &mut v,
            &AgentEvent::Delta {
                text: "answer".into(),
            },
            false,
        );
        assert!(v.activity_line().is_empty(), "{}", v.activity_line());
    }

    #[test]
    fn tool_failed_line_keeps_the_first_error_line() {
        assert_eq!(tool_failed_line("edit", "boom\nstack"), "  ✗ edit · boom");
        assert!(tool_line("web_fetch", "https://example.com/x").contains("reading"));
    }

    #[test]
    fn truncate_keeps_every_line_within_width() {
        // The ellipsis needs a column of its own: a character that would land
        // exactly in the last column yields to the mark, because the live
        // area's row math assumes no line exceeds `width`.
        assert_eq!(truncate_display("abcdXY", 5), "abcd…");
        // Nothing is truncated when the text fits, exact fit included.
        assert_eq!(truncate_display("abcdX", 5), "abcdX");
        assert_eq!(truncate_display("abcd", 5), "abcd");
        // Wide characters count two columns.
        assert_eq!(truncate_display("你好世界", 5), "你好…");
        assert!(truncate_display("你好世界", 5).width() <= 5);
    }
}
