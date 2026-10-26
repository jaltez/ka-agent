//! The line editor: a char buffer with a cursor, soft-wrapped rendering by
//! display width, history recall (prefix-filtered), Ctrl-R reverse search,
//! and the line-editing control keys. No external TUI dependency.

use unicode_width::UnicodeWidthChar;

/// One editor state (the draft the operator is typing).
#[derive(Default)]
pub struct Editor {
    chars: Vec<char>,
    cursor: usize,
    /// True while Ctrl-R searches (the prompt is drawn by the driver).
    pub searching: bool,
}

impl Editor {
    pub fn text(&self) -> String {
        self.chars.iter().collect()
    }

    pub fn is_empty(&self) -> bool {
        self.chars.is_empty()
    }

    #[cfg(test)]
    pub fn cursor(&self) -> usize {
        self.cursor
    }

    pub fn set(&mut self, text: &str) {
        self.chars = text.chars().collect();
        self.cursor = self.chars.len();
        self.searching = false;
    }

    pub fn clear(&mut self) {
        self.chars.clear();
        self.cursor = 0;
        self.searching = false;
    }

    pub fn insert(&mut self, c: char) {
        self.chars.insert(self.cursor, c);
        self.cursor += 1;
    }

    pub fn insert_str(&mut self, text: &str) {
        for c in text.chars() {
            // Pasted newlines become spaces: one line input, Enter submits.
            let c = if c == '\n' || c == '\r' { ' ' } else { c };
            self.insert(c);
        }
        self.after_edit();
    }

    pub fn backspace(&mut self) {
        if self.cursor > 0 {
            self.cursor -= 1;
            self.chars.remove(self.cursor);
            self.after_edit();
        }
    }

    pub fn delete(&mut self) {
        if self.cursor < self.chars.len() {
            self.chars.remove(self.cursor);
            self.after_edit();
        }
    }

    pub fn left(&mut self) {
        self.cursor = self.cursor.saturating_sub(1);
    }

    pub fn right(&mut self) {
        if self.cursor < self.chars.len() {
            self.cursor += 1;
        }
    }

    pub fn home(&mut self) {
        self.cursor = 0;
    }

    pub fn end(&mut self) {
        self.cursor = self.chars.len();
    }

    /// Move to the start of the previous word (Ctrl-Left / Alt-b).
    pub fn word_left(&mut self) {
        while self.cursor > 0 && self.chars[self.cursor - 1].is_whitespace() {
            self.cursor -= 1;
        }
        while self.cursor > 0 && !self.chars[self.cursor - 1].is_whitespace() {
            self.cursor -= 1;
        }
    }

    /// Move past the next word (Ctrl-Right / Alt-f).
    pub fn word_right(&mut self) {
        while self.cursor < self.chars.len() && self.chars[self.cursor].is_whitespace() {
            self.cursor += 1;
        }
        while self.cursor < self.chars.len() && !self.chars[self.cursor].is_whitespace() {
            self.cursor += 1;
        }
    }

    /// Ctrl-K: kill to the end.
    pub fn kill_to_end(&mut self) -> String {
        let killed: String = self.chars.drain(self.cursor..).collect();
        killed
    }

    /// Ctrl-U: kill to the start.
    pub fn kill_to_start(&mut self) -> String {
        let killed: String = self.chars.drain(..self.cursor).collect();
        self.cursor = 0;
        killed
    }

    /// Ctrl-W: kill the previous word.
    pub fn kill_word(&mut self) -> String {
        let end = self.cursor;
        self.word_left();
        let killed: String = self.chars.drain(self.cursor..end).collect();
        killed
    }

    // ---- display geometry -------------------------------------------

    /// Visual rows for `width` columns (wide chars count 2, wrapping by
    /// width). Returns the rows' char ranges plus the cursor's (row, col).
    pub fn layout(&self, width: usize) -> (Vec<(usize, usize)>, (usize, usize)) {
        let width = width.max(1);
        let mut rows: Vec<(usize, usize)> = Vec::new();
        let mut row_start = 0;
        let mut col = 0usize;
        let mut cursor_pos = (0usize, 0usize);
        for (i, c) in self.chars.iter().enumerate() {
            let w = c.width().unwrap_or(0);
            if col + w > width && col > 0 {
                rows.push((row_start, i));
                row_start = i;
                col = 0;
            }
            if i == self.cursor {
                cursor_pos = (rows.len(), col);
            }
            col += w;
        }
        rows.push((row_start, self.chars.len()));
        if self.cursor == self.chars.len() {
            let last = rows.len() - 1;
            let col = self
                .chars
                .get(rows[last].0..self.cursor)
                .map(|s| s.iter().map(|c| c.width().unwrap_or(0)).sum())
                .unwrap_or(0);
            cursor_pos = (last, col);
        }
        (rows, cursor_pos)
    }

    fn after_edit(&mut self) {
        // A search that is edited is abandoned; the text becomes the draft.
        self.searching = false;
    }
}

/// History: newest last, consecutive duplicates skipped, bounded.
pub struct History {
    lines: Vec<String>,
    /// Position while browsing (None = editing a fresh draft).
    pos: Option<usize>,
    /// The draft preserved while browsing.
    saved: String,
    path: std::path::PathBuf,
}

const HISTORY_MAX: usize = 1000;

impl History {
    pub fn load(path: std::path::PathBuf) -> Self {
        let mut lines = Vec::new();
        if let Ok(text) = std::fs::read_to_string(&path) {
            for line in text.lines() {
                if let Ok(s) = serde_json::from_str::<String>(line) {
                    lines.push(s);
                }
            }
        }
        if lines.len() > HISTORY_MAX {
            // The newest entries are the ones worth recalling.
            lines.drain(..lines.len() - HISTORY_MAX);
        }
        Self {
            lines,
            pos: None,
            saved: String::new(),
            path,
        }
    }

    pub fn push(&mut self, line: &str) {
        // A submission ends any walk: the next Up starts from the newest
        // entry and the draft saved by the old walk is gone.
        self.pos = None;
        self.saved.clear();
        if line.trim().is_empty() || self.lines.last().is_some_and(|l| l == line) {
            return;
        }
        self.lines.push(line.to_string());
        if self.lines.len() > HISTORY_MAX {
            self.lines.remove(0);
        }
        self.persist();
    }

    fn persist(&self) {
        use std::io::Write as _;
        let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(&self.path)
        else {
            return;
        };
        for line in &self.lines {
            if let Ok(enc) = serde_json::to_string(line) {
                let _ = writeln!(f, "{enc}");
            }
        }
    }

    /// Up: previous entry. `draft` is the editor's text and is only read
    /// when a walk starts (it is saved, and the whole walk filters by it,
    /// so later calls may pass the line currently shown).
    pub fn prev(&mut self, draft: &str) -> Option<String> {
        if self.lines.is_empty() {
            return None;
        }
        let start = match self.pos {
            Some(p) => p.checked_sub(1),
            None => {
                self.saved = draft.to_string();
                Some(self.lines.len().saturating_sub(1))
            }
        };
        let filter = self.saved.clone();
        let mut i = start;
        while let Some(idx) = i {
            if filter.is_empty() || self.lines[idx].starts_with(&filter) {
                self.pos = Some(idx);
                return Some(self.lines[idx].clone());
            }
            i = idx.checked_sub(1);
        }
        None
    }

    /// Down: next entry (same filter as the walk up), or the draft saved
    /// when the walk started, restoring what the operator was typing.
    pub fn next(&mut self) -> Option<String> {
        let pos = self.pos?;
        let mut i = pos + 1;
        while i < self.lines.len() {
            if self.saved.is_empty() || self.lines[i].starts_with(&self.saved) {
                self.pos = Some(i);
                return Some(self.lines[i].clone());
            }
            i += 1;
        }
        self.pos = None;
        Some(std::mem::take(&mut self.saved))
    }

    /// Reverse search hits for a query (newest first).
    pub fn search(&self, query: &str) -> Vec<String> {
        if query.is_empty() {
            return Vec::new();
        }
        let q = query.to_lowercase();
        self.lines
            .iter()
            .rev()
            .filter(|l| l.to_lowercase().contains(&q))
            .cloned()
            .collect()
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    #[test]
    fn insert_move_and_kill() {
        let mut e = Editor::default();
        e.insert_str("hello world");
        assert_eq!(e.cursor(), 11);
        e.home();
        assert_eq!(e.cursor(), 0);
        e.word_right();
        assert_eq!(e.cursor(), 5);
        e.delete();
        assert_eq!(e.text(), "helloworld");
        e.end();
        // One word: Ctrl-W takes the whole word.
        assert_eq!(e.kill_word(), "helloworld");
        assert_eq!(e.text(), "");
        e.insert_str(" big place");
        // The word before the cursor only; the space stays.
        assert_eq!(e.kill_word(), "place");
        assert_eq!(e.text(), " big ");
        assert_eq!(e.kill_to_start(), " big ");
        assert_eq!(e.text(), "");
    }

    #[test]
    fn layout_wraps_by_display_width() {
        let mut e = Editor::default();
        e.insert_str("abcdef");
        let (rows, cursor) = e.layout(4);
        assert_eq!(rows, vec![(0, 4), (4, 6)]);
        assert_eq!(cursor, (1, 2));
        // Wide chars count double.
        let mut w = Editor::default();
        w.insert_str("你好世界");
        let (rows, _) = w.layout(4);
        assert_eq!(rows, vec![(0, 2), (2, 4)]);
    }

    #[test]
    fn history_walks_and_filters_by_prefix() {
        let dir = std::env::temp_dir().join(format!("ka-hist-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut h = History::load(dir.join("history"));
        h.push("git status");
        h.push("cargo test");
        h.push("git log");
        h.push("git log"); // duplicate dropped
        assert_eq!(h.prev("git"), Some("git log".into()));
        assert_eq!(h.prev("git"), Some("git status".into()));
        assert_eq!(h.next(), Some("git log".into()));
        // Past the newest entry the draft saved at walk start comes back.
        assert_eq!(h.next(), Some("git".into()));
        assert_eq!(h.search("cargo"), vec!["cargo test".to_string()]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn history_restores_the_draft_not_an_empty_line() {
        let dir = std::env::temp_dir().join(format!("ka-hist-draft-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut h = History::load(dir.join("history"));
        h.push("cargo test");
        h.push("cargo build");
        // The driver passes the editor's text on every key: the draft first,
        // then the recalled line. Only the first one is the draft.
        assert_eq!(h.prev("car"), Some("cargo build".into()));
        assert_eq!(h.prev("cargo build"), Some("cargo test".into()));
        assert_eq!(h.prev("cargo test"), None);
        assert_eq!(h.next(), Some("cargo build".into()));
        let draft = h.next().unwrap();
        assert_eq!(draft, "car");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_submission_starts_the_next_walk_over() {
        let dir = std::env::temp_dir().join(format!("ka-hist-reset-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut h = History::load(dir.join("history"));
        h.push("one");
        h.push("two");
        assert_eq!(h.prev(""), Some("two".into()));
        // Submitting (even the recalled line) ends the walk.
        h.push("three");
        assert_eq!(h.prev(""), Some("three".into()));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn history_load_keeps_the_newest_entries() {
        let dir = std::env::temp_dir().join(format!("ka-hist-load-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("history");
        let mut body = String::new();
        for i in 0..HISTORY_MAX + 5 {
            body.push_str(&serde_json::to_string(&format!("line {i}")).unwrap());
            body.push('\n');
        }
        std::fs::write(&path, body).unwrap();
        let h = History::load(path);
        assert_eq!(h.lines.len(), HISTORY_MAX);
        assert_eq!(h.lines.first().map(String::as_str), Some("line 5"));
        assert_eq!(h.lines.last().map(String::as_str), Some("line 1004"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
