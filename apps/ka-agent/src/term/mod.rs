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
    /// Ctrl-C with an empty draft: stop the reply (or arm quit).
    Interrupt,
    /// Ctrl-D on an empty draft.
    Eof,
}

/// The live terminal: editor + view + history.
pub struct Terminal {
    _raw: raw::RawMode,
    key_rx: tokio::sync::mpsc::Receiver<Key>,
    pub editor: Editor,
    history: History,
    pub view: View,
    /// Quit armed by one Ctrl-C (a second within 2 s exits).
    quit_armed: Option<Instant>,
    /// Completion candidates currently shown (Tab again cycles).
    completions: Vec<complete::Completion>,
    completion_idx: usize,
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
        let mut term = Self {
            _raw: raw,
            key_rx: keys::spawn_reader(),
            editor: Editor::default(),
            history: History::load(history_path()),
            view,
            quit_armed: None,
            completions: Vec::new(),
            completion_idx: 0,
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
            let key = self.key_rx.recv().await?;
            // A resize or focus event only repaints.
            if let Key::Focus(focused) = key {
                self.view.focused = focused;
                self.draw();
                continue;
            }
            self.resize();
            match key {
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
                    println!("\x1b[1m»\x1b[0m\x1b[2m {}\x1b[0m", line.trim_end());
                    self.draw();
                    return Some(Action::Submit(line));
                }
                Key::Char(c) => {
                    self.editor.insert(c);
                    self.completions.clear();
                }
                Key::Newline => {
                    // Multi-line drafts are folded to spaces (single line).
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
                    if let Some(line) = self.history.next(&self.editor.text()) {
                        self.editor.set(&line);
                    }
                }
                Key::Home => self.editor.home(),
                Key::End => self.editor.end(),
                Key::WordLeft => self.editor.word_left(),
                Key::WordRight => self.editor.word_right(),
                Key::Tab => self.complete(),
                Key::BackTab => {}
                Key::Esc => self.completions.clear(),
                Key::Paste(text) => self.editor.insert_str(&text),
                Key::Ctrl('a') => self.editor.home(),
                Key::Ctrl('e') => self.editor.end(),
                Key::Ctrl('k') => {
                    self.editor.kill_to_end();
                }
                Key::Ctrl('u') => {
                    self.editor.kill_to_start();
                }
                Key::Ctrl('w') => {
                    self.editor.kill_word();
                }
                Key::Ctrl('l') => {
                    // Clear the screen, keep the live area.
                    print!("\x1b[2J\x1b[H");
                }
                Key::Ctrl('o') => {
                    self.view.verbose = !self.view.verbose;
                    print!(
                        "\x1b[2m· tool details {}\x1b[0m\r\n",
                        if self.view.verbose { "on" } else { "off" }
                    );
                }
                Key::Ctrl('t') => {
                    self.verbosity = !self.verbosity;
                    print!(
                        "\x1b[2m· thinking {}\x1b[0m\r\n",
                        if self.verbosity { "shown" } else { "hidden" }
                    );
                }
                Key::Ctrl('r') => {
                    // Incremental reverse search over the history.
                    self.reverse_search();
                }
                Key::Ctrl('g') => {
                    self.edit_in_editor();
                }
                Key::Ctrl('c') => {
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
                                print!("\r\n\x1b[2m(press Ctrl-C again to quit)\x1b[0m");
                            }
                        }
                    }
                }
                Key::Interrupt => {
                    if busy {
                        self.draw();
                        return Some(Action::Interrupt);
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
            self.draw();
        }
    }

    /// Tab: complete the current word; one match inserts, several list.
    fn complete(&mut self) {
        let workspace = self.view_workspace();
        let candidates = complete::complete(&self.editor.text(), &workspace);
        if candidates.is_empty() {
            return;
        }
        if candidates.len() == 1 {
            self.replace_word(&candidates[0].insert);
            self.completions.clear();
            return;
        }
        self.completion_idx = (self.completion_idx + 1) % candidates.len();
        self.completions = candidates;
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
        print!(
            "\x1b[2m{}\x1b[0m",
            view::truncate_display(&shared, self.width)
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

    /// Ctrl-R: incremental reverse search; Enter picks, Ctrl-G/Esc cancels.
    fn reverse_search(&mut self) {
        let mut query = String::new();
        loop {
            let hits = self.history.search(&query);
            let shown = hits.first().cloned().unwrap_or_default();
            print!("\r\x1b[2m(reverse-i-search)`{query}':\x1b[0m {shown}\x1b[K");
            let Some(key) = self.key_rx.blocking_recv() else {
                return;
            };
            match key {
                Key::Char(c) => query.push(c),
                Key::Backspace => {
                    query.pop();
                }
                Key::Enter => {
                    if !shown.is_empty() {
                        self.editor.set(&shown);
                    }
                    return;
                }
                Key::Esc | Key::Ctrl('g') | Key::Interrupt => return,
                _ => {}
            }
            if query.is_empty() {
                return;
            }
        }
    }

    /// Ctrl-G: edit the draft in $VISUAL/$EDITOR (raw mode suspended).
    fn edit_in_editor(&mut self) {
        let dir = std::env::temp_dir();
        let path = dir.join(format!("ka-draft-{}.md", std::process::id()));
        let _ = std::fs::write(&path, self.editor.text());
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
        if let Ok(text) = std::fs::read_to_string(&path) {
            self.editor.set(text.trim_end());
        }
        let _ = std::fs::remove_file(&path);
        self.draw();
    }
}

/// Terminal width (falls back to 80).
fn terminal_size_width() -> usize {
    raw::width()
}

/// Feed one envelope into the view and print its permanent lines above the
/// live area. Also rings BEL + OSC-9 when a card arrives while unfocused.
pub fn show_event(term: &mut Terminal, env: &ka_agent_runtime::Envelope) {
    use ka_agent_runtime::AgentEvent;
    let lines = view::absorb(&mut term.view, &env.ev, term.verbosity);
    // Print permanent output where the live area was: replace the area, then
    // redraw it below the new lines.
    term.begin_permanent();
    for line in lines {
        println!("{line}");
    }
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
