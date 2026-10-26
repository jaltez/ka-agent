//! Drawing commands: the live area is built as a small op list once, and
//! each platform prints it — raw ANSI on Unix/macOS (one write, one flush),
//! console commands through `crossterm` on Windows (plan 4.4: the console
//! does not interpret ANSI unless VT processing is enabled, and enabling it
//! needs `unsafe` FFI the workspace forbids).
//!
//! Keeping one op list means the layout is tested once and both printers
//! stay trivial.

/// One drawing command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Op {
    /// Plain text.
    Text(String),
    /// Dim text (`ESC[2m`).
    Dim(String),
    /// Bold text (`ESC[1m`).
    Bold(String),
    /// Carriage return, column 0.
    Cr,
    /// Move the cursor up `n` rows.
    Up(usize),
    /// Move the cursor right `n` columns.
    Right(usize),
    /// Clear from the cursor to the end of the screen.
    ClearDown,
    /// Begin a synchronized update.
    SyncBegin,
    /// End a synchronized update.
    SyncEnd,
}

/// Print the ops (one buffered write, one flush).
pub fn emit(ops: &[Op]) {
    #[cfg(unix)]
    {
        use std::io::Write as _;
        let mut out = String::new();
        for op in ops {
            match op {
                Op::Text(s) => out.push_str(s),
                Op::Dim(s) => {
                    out.push_str("\x1b[2m");
                    out.push_str(s);
                    out.push_str("\x1b[0m");
                }
                Op::Bold(s) => {
                    out.push_str("\x1b[1m");
                    out.push_str(s);
                    out.push_str("\x1b[0m");
                }
                Op::Cr => out.push('\r'),
                Op::Up(n) => out.push_str(&format!("\x1b[{n}A")),
                Op::Right(n) => out.push_str(&format!("\x1b[{n}C")),
                Op::ClearDown => out.push_str("\x1b[J"),
                Op::SyncBegin => out.push_str("\x1b[?2026h"),
                Op::SyncEnd => out.push_str("\x1b[?2026l"),
            }
        }
        let mut stdout = std::io::stdout();
        let _ = stdout.write_all(out.as_bytes());
        let _ = stdout.flush();
    }
    #[cfg(windows)]
    {
        use crossterm::style::{Attribute, SetAttribute};
        use crossterm::{cursor, queue, terminal};
        let mut stdout = std::io::stdout();
        for op in ops {
            let _ = match op {
                Op::Text(s) => queue!(stdout, crossterm::style::Print(s)),
                Op::Dim(s) => queue!(
                    stdout,
                    SetAttribute(Attribute::Dim),
                    crossterm::style::Print(s),
                    SetAttribute(Attribute::Reset)
                ),
                Op::Bold(s) => queue!(
                    stdout,
                    SetAttribute(Attribute::Bold),
                    crossterm::style::Print(s),
                    SetAttribute(Attribute::Reset)
                ),
                Op::Cr => queue!(stdout, cursor::MoveToColumn(0)),
                Op::Up(n) => queue!(stdout, cursor::MoveUp(*n as u16)),
                Op::Right(n) => queue!(stdout, cursor::MoveRight(*n as u16)),
                Op::ClearDown => {
                    queue!(stdout, terminal::Clear(terminal::ClearType::FromCursorDown))
                }
                Op::SyncBegin => queue!(stdout, terminal::BeginSynchronizedUpdate),
                Op::SyncEnd => queue!(stdout, terminal::EndSynchronizedUpdate),
            };
        }
        use std::io::Write as _;
        let _ = stdout.flush();
    }
}

/// Render ops to the Unix ANSI byte string (tests and the Unix printer).
#[cfg_attr(not(test), allow(dead_code))]
pub fn to_ansi(ops: &[Op]) -> String {
    let mut out = String::new();
    for op in ops {
        match op {
            Op::Text(s) => out.push_str(s),
            Op::Dim(s) => {
                out.push_str("\x1b[2m");
                out.push_str(s);
                out.push_str("\x1b[0m");
            }
            Op::Bold(s) => {
                out.push_str("\x1b[1m");
                out.push_str(s);
                out.push_str("\x1b[0m");
            }
            Op::Cr => out.push('\r'),
            Op::Up(n) => out.push_str(&format!("\x1b[{n}A")),
            Op::Right(n) => out.push_str(&format!("\x1b[{n}C")),
            Op::ClearDown => out.push_str("\x1b[J"),
            Op::SyncBegin => out.push_str("\x1b[?2026h"),
            Op::SyncEnd => out.push_str("\x1b[?2026l"),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    #[test]
    fn ansi_rendering_is_the_established_bytes() {
        assert_eq!(
            to_ansi(&[
                Op::SyncBegin,
                Op::Up(2),
                Op::Cr,
                Op::ClearDown,
                Op::Dim("status".into()),
                Op::Text("\r\n".into()),
                Op::Bold("»".into()),
                Op::Text(" hi".into()),
                Op::Right(3),
                Op::SyncEnd,
            ]),
            "\x1b[?2026h\x1b[2A\r\x1b[J\x1b[2mstatus\x1b[0m\r\n\x1b[1m»\x1b[0m hi\x1b[3C\x1b[?2026l"
        );
    }
}
