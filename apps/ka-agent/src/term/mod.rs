//! The inline terminal client (plan 3.1): raw mode + a real line editor +
//! a live status line, drawn above a single shared Markdown renderer. Only
//! used when stdin and stdout are TTYs; the piped path stays line-based.

pub mod cards;
pub mod complete;
pub mod editor;
pub mod keys;
pub mod paint;
pub mod raw;
pub mod view;

use std::time::{Duration, Instant};

use editor::{Editor, History};
use keys::Key;
use view::View;

/// Where the draft's history lives.
fn history_path() -> std::path::PathBuf {
    ka_agent_runtime::paths::Paths::new(ka_agent_runtime::paths::Paths::default_root())
        .root()
        .join("chat_history")
}

/// One resolved input step for the chat loop.
pub enum Action {
    /// The operator submitted this line.
    Submit(String),
    /// Ctrl-C on an empty draft while a turn runs: stop the reply.
    Interrupt,
    /// Ctrl-D on an empty draft.
    Eof,
}

/// The live terminal: editor + view + history.
pub struct Terminal {
    _raw: raw::RawMode,
    key_rx: tokio::sync::mpsc::Receiver<Key>,
    /// Pauses the stdin reader while an external editor runs (Ctrl-G).
    keys_pause: keys::Pause,
    pub editor: Editor,
    history: History,
    pub view: View,
    /// Quit armed by one Ctrl-C (a second within 2 s exits).
    quit_armed: Option<Instant>,
    /// Completion candidates currently shown (Tab again cycles).
    completions: Vec<complete::Completion>,
    completion_idx: usize,
    /// Where the cycling word starts in the draft (char index).
    completion_start: Option<usize>,
    /// Ctrl-R query while the reverse search runs.
    search: Option<String>,
    pub verbosity: bool,
    /// Where `@path` completion and the editor's cwd resolve.
    workspace: std::path::PathBuf,
    width: usize,
    /// Rows between the live area's first row and the cursor (0 = the
    /// cursor is on the first row): the next redraw moves up exactly this
    /// far before clearing, so no stale rows remain.
    cursor_up: usize,
}

impl Terminal {
    /// Enter the terminal. `None` when the session is not a TTY or raw mode
    /// cannot be enabled (the caller falls back to the line path).
    pub fn start(workspace: std::path::PathBuf, name: String, model: String) -> Option<Self> {
        if !raw::is_tty() {
            return None;
        }
        let raw = raw::enter()?;
        let width = terminal_size_width();
        let mut view = View::new(name, width);
        view.model = model;
        let (key_rx, keys_pause) = keys::spawn_reader();
        let mut term = Self {
            _raw: raw,
            key_rx,
            keys_pause,
            editor: Editor::default(),
            history: History::load(history_path()),
            view,
            quit_armed: None,
            completions: Vec::new(),
            completion_idx: 0,
            completion_start: None,
            search: None,
            verbosity: false,
            workspace,
            width,
            cursor_up: 0,
        };
        term.resize();
        term.draw();
        Some(term)
    }

    fn resize(&mut self) {
        self.width = terminal_size_width();
        self.view.width = self.width;
    }

    pub fn draw(&mut self) {
        if self.view.running {
            self.view.tick();
        }
        self.cursor_up = self.view.redraw(&self.editor, self.cursor_up);
    }

    /// Move to the live area's first row and clear it down: the caller then
    /// prints permanent lines from there, and the next `draw` starts fresh.
    pub fn begin_permanent(&mut self) {
        use std::io::Write as _;
        let mut out = String::new();
        for _ in 0..self.cursor_up {
            out.push_str("\x1b[1A");
        }
        out.push_str("\r\x1b[J");
        let mut stdout = std::io::stdout();
        let _ = stdout.write_all(out.as_bytes());
        let _ = stdout.flush();
        self.cursor_up = 0;
    }

    /// Next resolved input step; editing keys are handled here, redrawing
    /// as needed. `busy` = a turn is running (Ctrl-C stops it).
    pub async fn next_action(&mut self, busy: bool) -> Option<Action> {
        loop {
            if self.search.is_some() {
                self.draw_search();
            }
            let key = self.key_rx.recv().await?;
            // A resize or focus event only repaints.
            if let Key::Focus(focused) = key {
                self.view.focused = focused;
                self.draw();
                continue;
            }
            self.resize();
            // Ctrl-R runs as this state machine (never a blocking recv —
            // the async runtime owns this thread).
            if self.search.is_some() {
                if !search_key(&mut self.search, &self.history, &mut self.editor, key) {
                    self.draw();
                }
                continue;
            }
            match &key {
                Key::Enter => {
                    let line = self.editor.text();
                    if line.trim().is_empty() {
                        self.editor.clear();
                        self.draw();
                        continue;
                    }
                    self.history.push(&line);
                    self.editor.clear();
                    self.completions.clear();
                    self.quit_armed = None;
                    // The submitted line enters the transcript permanently.
                    self.begin_permanent();
                    println!(
                        "{}{}",
                        paint::bold("»"),
                        paint::dim(&format!(" {}", line.trim_end()))
                    );
                    self.draw();
                    return Some(Action::Submit(line));
                }
                Key::Char(c) => {
                    self.editor.insert(*c);
                }
                Key::Newline => {
                    // A typed newline folds to a space (one-line input);
                    // pasted newlines survive verbatim in the draft.
                    self.editor.insert(' ');
                }
                Key::Backspace => self.editor.backspace(),
                Key::Delete => self.editor.delete(),
                Key::Left => self.editor.left(),
                Key::Right => self.editor.right(),
                Key::Up => {
                    if let Some(line) = self.history.prev(&self.editor.text()) {
                        self.editor.set(&line);
                    }
                }
                Key::Down => {
                    if let Some(line) = self.history.next() {
                        self.editor.set(&line);
                    }
                }
                Key::Home => self.editor.home(),
                Key::End => self.editor.end(),
                Key::WordLeft => self.editor.word_left(),
                Key::WordRight => self.editor.word_right(),
                Key::Tab => self.complete(),
                Key::BackTab => {}
                Key::Esc => {
                    // While a turn runs, Esc stops it (the engine's own
                    // retry notes say so); idle it dismisses completions.
                    if busy {
                        self.draw();
                        return Some(Action::Interrupt);
                    }
                    self.completions.clear();
                }
                Key::Paste(text) => self.editor.insert_str(text),
                Key::Ctrl('a') => self.editor.home(),
                Key::Ctrl('e') => self.expand_tool(),
                Key::Ctrl('k') => {
                    self.editor.kill_to_end();
                }
                Key::Ctrl('u') => {
                    self.editor.kill_to_start();
                }
                Key::Ctrl('w') => {
                    self.editor.kill_word();
                }
                Key::Ctrl('y') => self.editor.yank(),
                Key::Ctrl('l') => {
                    // Clear the screen, keep the live area.
                    print!("\x1b[2J\x1b[H");
                }
                Key::Ctrl('o') => {
                    self.view.verbose = !self.view.verbose;
                    self.announce(&format!(
                        "tool details {}",
                        if self.view.verbose { "on" } else { "off" }
                    ));
                }
                Key::Ctrl('t') => {
                    self.verbosity = !self.verbosity;
                    self.announce(&format!(
                        "thinking {}",
                        if self.verbosity { "shown" } else { "hidden" }
                    ));
                }
                Key::Ctrl('r') => {
                    // Incremental reverse search over the history: the
                    // `search_key` state machine takes the next keys.
                    self.search = Some(String::new());
                    self.editor.searching = true;
                }
                Key::Ctrl('g') => {
                    self.edit_in_editor();
                }
                Key::Interrupt => {
                    // Ctrl-C (Unix 0x03, Windows the console event): a
                    // non-empty draft is cleared, an empty one stops the
                    // running turn, and twice on an idle prompt quits.
                    if !self.editor.is_empty() {
                        self.editor.clear();
                        self.completions.clear();
                    } else if busy {
                        self.draw();
                        return Some(Action::Interrupt);
                    } else {
                        match self.quit_armed.take() {
                            Some(at) if at.elapsed() < Duration::from_secs(2) => {
                                println!();
                                return Some(Action::Eof);
                            }
                            _ => {
                                self.quit_armed = Some(Instant::now());
                                print!("\r\n{}", paint::dim("(press Ctrl-C again to quit)"));
                            }
                        }
                    }
                }
                Key::Eof => {
                    if self.editor.is_empty() {
                        println!();
                        return Some(Action::Eof);
                    }
                }
                Key::Ctrl(_) => {}
                Key::Focus(_) => {}
            }
            // Any key but Tab ends a completion cycle.
            if !matches!(key, Key::Tab) {
                self.completions.clear();
                self.completion_start = None;
            }
            self.draw();
        }
    }

    /// One dim line above the live area (survives the next redraw).
    fn announce(&mut self, text: &str) {
        self.begin_permanent();
        println!("{}", paint::dim(&format!("· {text}")));
    }

    /// Tab: complete the current word. A single match inserts; several
    /// arm a cycle — the first candidate inserts, further Tabs step through
    /// the list, and the candidates print as a dim row above the live area
    /// (so they survive the redraw instead of being erased by it).
    fn complete(&mut self) {
        if self.completions.is_empty() {
            let workspace = self.view_workspace();
            let candidates = complete::complete(&self.editor.text(), &workspace);
            if candidates.is_empty() {
                return;
            }
            if candidates.len() == 1 {
                self.replace_word(&candidates[0].insert);
                return;
            }
            let text = self.editor.text();
            let start = text
                .rfind(char::is_whitespace)
                .map_or(0, |i| text[..i + 1].chars().count());
            self.completions = candidates;
            self.completion_idx = 0;
            self.completion_start = Some(start);
        } else {
            self.completion_idx = (self.completion_idx + 1) % self.completions.len();
        }
        self.insert_completion();
        self.show_completions();
    }

    /// Replace the word at `completion_start` with the current candidate.
    fn insert_completion(&mut self) {
        let Some(start) = self.completion_start else {
            return;
        };
        let insert = self.completions[self.completion_idx].insert.clone();
        let head: String = self.editor.text().chars().take(start).collect();
        self.editor.set(&format!("{head}{insert} "));
    }

    /// The candidates as one dim row above the live area.
    fn show_completions(&mut self) {
        let shared = self
            .completions
            .iter()
            .map(|c| {
                if c.hint.is_empty() || c.insert.starts_with('@') {
                    c.insert.clone()
                } else {
                    format!("{} ({})", c.insert, c.hint)
                }
            })
            .collect::<Vec<_>>()
            .join("  ");
        self.begin_permanent();
        println!(
            "{}",
            paint::dim(&view::truncate_display(&shared, self.width))
        );
    }

    fn replace_word(&mut self, insert: &str) {
        let text = self.editor.text();
        let start = text.rfind(char::is_whitespace).map(|i| i + 1).unwrap_or(0);
        let head = &text[..start];
        self.editor.set(&format!("{head}{insert} "));
    }

    fn view_workspace(&self) -> std::path::PathBuf {
        self.workspace.clone()
    }

    /// The Ctrl-R prompt: `(reverse-i-search)`query': match` on the
    /// cursor's row (the live area redraws over it when the search ends).
    fn draw_search(&self) {
        let Some(query) = &self.search else {
            return;
        };
        let shown = self
            .history
            .search(query)
            .first()
            .cloned()
            .unwrap_or_default();
        print!(
            "\r{} {}\x1b[K",
            paint::dim(&format!("(reverse-i-search)`{query}':")),
            shown
        );
        use std::io::Write as _;
        let _ = std::io::stdout().flush();
    }

    /// Ctrl-E: reprint the last finished tool — its invocation, duration
    /// and the output the runtime surfaced (a failed call's error lines, a
    /// successful call's bounded output excerpt).
    fn expand_tool(&mut self) {
        let Some(last) = self.view.last_tool.clone() else {
            self.announce("no tool yet");
            return;
        };
        self.begin_permanent();
        println!(
            "{}",
            paint::dim(&format!(
                "· {} · {}ms · {}",
                last.tool,
                last.ms,
                if last.ok { "ok" } else { "failed" }
            ))
        );
        if !last.detail.is_empty() {
            println!(
                "{}",
                paint::dim(&format!(
                    "  ▸ {}",
                    view::truncate_display(&last.detail, self.width.saturating_sub(4))
                ))
            );
        }
        if let Some(err) = &last.error {
            for line in err.lines() {
                println!("{}", paint::dim(&format!("  {line}")));
            }
        }
        if let Some(excerpt) = &last.output {
            for line in excerpt.lines().take(20) {
                println!("{}", paint::dim(&format!("  {line}")));
            }
        }
    }

    /// Ctrl-G: edit the draft in $VISUAL/$EDITOR (raw mode suspended and
    /// the stdin reader paused, so the editor owns every keystroke).
    fn edit_in_editor(&mut self) {
        let dir = std::env::temp_dir();
        let path = dir.join(format!("ka-draft-{}.md", std::process::id()));
        let _ = std::fs::write(&path, self.editor.text());
        self.keys_pause.set(true);
        let suspended = raw::Suspended::begin();
        let editor = std::env::var("VISUAL")
            .or_else(|_| std::env::var("EDITOR"))
            .unwrap_or_else(|_| {
                if cfg!(windows) {
                    "notepad".to_string()
                } else {
                    "vi".to_string()
                }
            });
        let _ = std::process::Command::new(&editor).arg(&path).status();
        suspended.end();
        self.keys_pause.set(false);
        // Whatever the reader swallowed around the switch is stale.
        while self.key_rx.try_recv().is_ok() {}
        if let Ok(text) = std::fs::read_to_string(&path) {
            self.editor.set(text.trim_end());
        }
        let _ = std::fs::remove_file(&path);
        self.draw();
    }
}

/// One key inside the Ctrl-R search: `true` while it keeps running.
/// Enter accepts the current hit into the draft, Esc/Ctrl-G/Ctrl-C cancel,
/// backspacing the query away ends the search. Free so the state machine
/// is unit-testable without a terminal.
fn search_key(
    search: &mut Option<String>,
    history: &History,
    editor: &mut Editor,
    key: Key,
) -> bool {
    let Some(query) = search.as_mut() else {
        return false;
    };
    match key {
        Key::Char(c) => query.push(c),
        Key::Backspace => {
            query.pop();
        }
        Key::Enter => {
            if let Some(line) = history.search(query).into_iter().next() {
                editor.set(&line);
            }
            *search = None;
            return false;
        }
        Key::Esc | Key::Ctrl('g') | Key::Interrupt => {
            *search = None;
            return false;
        }
        _ => {}
    }
    if query.is_empty() {
        *search = None;
        return false;
    }
    true
}

/// Terminal width (falls back to 80).
fn terminal_size_width() -> usize {
    raw::width()
}

/// Feed one envelope into the view and print its permanent lines above the
/// live area — cards included, so an approval card is a permanent block
/// (never overwritten by the redraw its arrival triggers). Also rings BEL
/// + OSC-9 when a card arrives while unfocused.
pub fn show_event(term: &mut Terminal, env: &ka_agent_runtime::Envelope) {
    use ka_agent_runtime::AgentEvent;
    let lines = view::absorb(&mut term.view, &env.ev, term.verbosity);
    // Print permanent output where the live area was: replace the area, then
    // redraw it below the new lines.
    term.begin_permanent();
    for line in lines {
        println!("{line}");
    }
    cards::print_card(env);
    match &env.ev {
        AgentEvent::Approval { question, .. }
        | AgentEvent::MemoryProposal { text: question, .. } => {
            if !term.view.focused {
                print!("\x07\x1b]9;{}: {}\x07", term.view.name, question);
            }
        }
        AgentEvent::TurnFinished { ms, .. } if *ms > 10_000 && !term.view.focused => {
            print!("\x07\x1b]9;{}: reply ready\x07", term.view.name);
        }
        _ => {}
    }
    term.draw();
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    /// A2: the reverse-search state machine accepts, cancels and exits on
    /// an emptied query — the same keys the blocking loop served before,
    /// without ever blocking the runtime thread.
    #[test]
    fn reverse_search_state_machine() {
        let dir = std::env::temp_dir().join(format!("ka-search-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut history = History::load(dir.join("history"));
        history.push("hello there");
        history.push("bye now");

        // `hel` + Enter: the hit lands in the draft, the search ends.
        let mut editor = Editor::default();
        let mut search = Some(String::new());
        assert!(search_key(
            &mut search,
            &history,
            &mut editor,
            Key::Char('h')
        ));
        assert!(search_key(
            &mut search,
            &history,
            &mut editor,
            Key::Char('e')
        ));
        assert!(search_key(
            &mut search,
            &history,
            &mut editor,
            Key::Char('l')
        ));
        assert!(!search_key(&mut search, &history, &mut editor, Key::Enter));
        assert_eq!(editor.text(), "hello there");
        assert!(search.is_none());

        // Esc cancels: the draft keeps whatever it held.
        editor.set("draft");
        let mut search = Some(String::new());
        assert!(search_key(
            &mut search,
            &history,
            &mut editor,
            Key::Char('b')
        ));
        assert!(!search_key(&mut search, &history, &mut editor, Key::Esc));
        assert_eq!(editor.text(), "draft");

        // Backspacing the query away exits the search.
        let mut search = Some(String::new());
        assert!(search_key(
            &mut search,
            &history,
            &mut editor,
            Key::Char('x')
        ));
        assert!(!search_key(
            &mut search,
            &history,
            &mut editor,
            Key::Backspace
        ));
        assert!(search.is_none());

        // Ctrl-G and Ctrl-C cancel too.
        let mut search = Some(String::new());
        assert!(search_key(
            &mut search,
            &history,
            &mut editor,
            Key::Char('b')
        ));
        assert!(!search_key(
            &mut search,
            &history,
            &mut editor,
            Key::Ctrl('g')
        ));
        let mut search = Some(String::new());
        assert!(search_key(
            &mut search,
            &history,
            &mut editor,
            Key::Char('b')
        ));
        assert!(!search_key(
            &mut search,
            &history,
            &mut editor,
            Key::Interrupt
        ));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
