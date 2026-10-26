//! Key decoding for the inline terminal client: a dedicated std thread
//! reads stdin bytes, decodes UTF-8 characters and the escape sequences the
//! editor binds, and forwards [`Key`] events. Split reads are buffered; a
//! lone ESC is only emitted after a short pause (an arrow key arrives whole).

use tokio::sync::mpsc;

/// One decoded key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Key {
    Char(char),
    Enter,
    /// Ctrl-J / Alt-Enter / Shift-Enter: newline inside the draft.
    Newline,
    Backspace,
    Delete,
    Left,
    Right,
    Up,
    Down,
    Home,
    End,
    WordLeft,
    WordRight,
    Tab,
    BackTab,
    Esc,
    /// Ctrl-C.
    Interrupt,
    /// Ctrl-D.
    Eof,
    /// Ctrl-A/E/K/U/W/Y/L/O/R/T/G and friends, passed through by letter.
    Ctrl(char),
    /// Bracketed paste payload.
    Paste(String),
    /// The terminal gained/lost focus.
    Focus(bool),
}

/// Decode console input until EOF; every complete key is sent. The
/// channel closes when the input ends.
#[cfg(windows)]
pub fn spawn_reader() -> mpsc::Receiver<Key> {
    use crossterm::event::{Event, KeyEvent, KeyEventKind};
    let (tx, rx) = mpsc::channel(256);
    std::thread::spawn(move || {
        loop {
            let event = match crossterm::event::read() {
                Ok(event) => event,
                Err(_) => return,
            };
            let key = match event {
                Event::Paste(text) => Some(Key::Paste(text)),
                Event::FocusGained => Some(Key::Focus(true)),
                Event::FocusLost => Some(Key::Focus(false)),
                Event::Key(KeyEvent {
                    code,
                    modifiers,
                    kind,
                    ..
                }) => {
                    if kind == KeyEventKind::Release {
                        continue;
                    }
                    Some(map_key(code, modifiers))
                }
                _ => None,
            };
            if let Some(key) = key
                && tx.blocking_send(key).is_err()
            {
                return;
            }
        }
    });
    rx
}

/// A Windows console key event → the shared [`Key`] vocabulary.
#[cfg(windows)]
fn map_key(code: crossterm::event::KeyCode, modifiers: crossterm::event::KeyModifiers) -> Key {
    use crossterm::event::{KeyCode, KeyModifiers};
    let ctrl = modifiers.contains(KeyModifiers::CONTROL);
    let alt = modifiers.contains(KeyModifiers::ALT);
    let shift = modifiers.contains(KeyModifiers::SHIFT);
    match code {
        KeyCode::Enter => {
            if alt || shift {
                Key::Newline
            } else {
                Key::Enter
            }
        }
        KeyCode::Char(c) if ctrl => {
            if alt {
                match c.to_ascii_lowercase() {
                    'b' => Key::WordLeft,
                    'f' => Key::WordRight,
                    _ => Key::Ctrl(c),
                }
            } else {
                let lower = c.to_ascii_lowercase();
                match lower {
                    'c' => Key::Interrupt,
                    'd' => Key::Eof,
                    'j' => Key::Newline,
                    other => Key::Ctrl(other),
                }
            }
        }
        KeyCode::Char(c) if alt => match c.to_ascii_lowercase() {
            'b' => Key::WordLeft,
            'f' => Key::WordRight,
            _ => Key::Char(c),
        },
        KeyCode::Char(c) => Key::Char(c),
        KeyCode::Backspace => Key::Backspace,
        KeyCode::Delete => Key::Delete,
        KeyCode::Left => {
            if ctrl {
                Key::WordLeft
            } else {
                Key::Left
            }
        }
        KeyCode::Right => {
            if ctrl {
                Key::WordRight
            } else {
                Key::Right
            }
        }
        KeyCode::Up => Key::Up,
        KeyCode::Down => Key::Down,
        KeyCode::Home => Key::Home,
        KeyCode::End => Key::End,
        KeyCode::Tab => Key::Tab,
        KeyCode::BackTab => Key::BackTab,
        KeyCode::Esc => Key::Esc,
        _ => Key::Esc,
    }
}

/// Decode stdin until EOF; every complete key is sent. `None` payload means
/// the stream closed.
#[cfg(unix)]
pub fn spawn_reader() -> mpsc::Receiver<Key> {
    let (tx, rx) = mpsc::channel(256);
    std::thread::spawn(move || {
        use std::io::Read as _;
        let mut stdin = std::io::stdin();
        let mut buf = [0u8; 1024];
        let mut pending: Vec<u8> = Vec::new();
        loop {
            let n = match stdin.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => n,
                Err(_) => break,
            };
            pending.extend_from_slice(&buf[..n]);
            // Decode everything that is complete; leave a trailing partial
            // UTF-8 char or an unfinished ESC sequence for the next read.
            let mut idx = 0;
            while idx < pending.len() {
                match decode_one(&pending[idx..]) {
                    Decoded::Key(key, used) => {
                        if tx.blocking_send(key).is_err() {
                            return;
                        }
                        idx += used;
                    }
                    Decoded::NeedMore => break,
                }
            }
            pending.drain(..idx);
            if pending.len() > 4096 {
                // Not a key stream any more; drop it rather than grow.
                pending.clear();
            }
        }
    });
    rx
}

#[cfg(unix)]
enum Decoded {
    Key(Key, usize),
    NeedMore,
}

/// Decode one key at the head of `bytes`.
#[cfg(unix)]
fn decode_one(bytes: &[u8]) -> Decoded {
    let b = bytes[0];
    match b {
        0x1b => decode_escape(bytes),
        b'\r' => Decoded::Key(Key::Enter, 1),
        b'\n' => Decoded::Key(Key::Newline, 1),
        0x7f | 0x08 => Decoded::Key(Key::Backspace, 1),
        b'\t' => Decoded::Key(Key::Tab, 1),
        0x03 => Decoded::Key(Key::Interrupt, 1),
        0x04 => Decoded::Key(Key::Eof, 1),
        0x01..=0x1a => Decoded::Key(Key::Ctrl((b - 1 + b'a') as char), 1),
        0x00..=0x1f => Decoded::NeedMore, // other control bytes: ignore
        _ => {
            // UTF-8: find the char length from the leading byte.
            let len = match b {
                0x00..=0x7f => 1,
                0xc0..=0xdf => 2,
                0xe0..=0xef => 3,
                0xf0..=0xf7 => 4,
                _ => return Decoded::Key(Key::Char('\u{fffd}'), 1),
            };
            if bytes.len() < len {
                return Decoded::NeedMore;
            }
            match std::str::from_utf8(&bytes[..len]) {
                Ok(s) => Decoded::Key(Key::Char(s.chars().next().unwrap_or('\u{fffd}')), len),
                Err(_) => Decoded::Key(Key::Char('\u{fffd}'), 1),
            }
        }
    }
}

/// ESC-prefixed sequences: arrows, Home/End, Delete, word moves, Alt-Enter,
/// Shift-Enter, bracketed paste, focus in/out, Alt-b/Alt-f.
#[cfg(unix)]
fn decode_escape(bytes: &[u8]) -> Decoded {
    if bytes.len() == 1 {
        return Decoded::NeedMore;
    }
    match bytes[1] {
        b'[' => decode_csi(bytes),
        b'\r' => Decoded::Key(Key::Newline, 2), // Alt-Enter
        b'b' => Decoded::Key(Key::WordLeft, 2),
        b'f' => Decoded::Key(Key::WordRight, 2),
        b'O' => {
            if bytes.len() < 3 {
                return Decoded::NeedMore;
            }
            let key = match bytes[2] {
                b'A' => Key::Up,
                b'B' => Key::Down,
                b'C' => Key::Right,
                b'D' => Key::Left,
                b'H' => Key::Home,
                b'F' => Key::End,
                _ => Key::Esc,
            };
            Decoded::Key(key, 3)
        }
        _ => Decoded::Key(Key::Esc, 1),
    }
}

#[cfg(unix)]
fn decode_csi(bytes: &[u8]) -> Decoded {
    // ESC [ … <final byte in 0x40..=0x7e>
    let mut i = 2;
    while i < bytes.len() && !(0x40..=0x7e).contains(&bytes[i]) {
        i += 1;
    }
    if i >= bytes.len() {
        // Bracketed paste is bounded by its end marker, not a final byte:
        // `ESC [ 2 0 0 ~` IS complete (the ~ is the final byte).
        return Decoded::NeedMore;
    }
    let params = &bytes[2..i];
    let fin = bytes[i];
    let used = i + 1;
    let key = match (fin, params) {
        (b'A', _) => Key::Up,
        (b'B', _) => Key::Down,
        (b'C', b"1;5") | (b'C', b"1;3") => Key::WordRight,
        (b'D', b"1;5") | (b'D', b"1;3") => Key::WordLeft,
        (b'C', _) => Key::Right,
        (b'D', _) => Key::Left,
        (b'H', _) => Key::Home,
        (b'F', _) => Key::End,
        (b'Z', _) => Key::BackTab,
        (b'~', b"1") | (b'~', b"7") => Key::Home,
        (b'~', b"4") | (b'~', b"8") => Key::End,
        (b'~', b"3") => Key::Delete,
        (b'~', b"200") => return decode_paste(bytes, used),
        (b'~', b"13;2") => Key::Newline, // Shift-Enter (kitty/modifyOtherKeys)
        (b'u', b"13") | (b'u', b"13;1") => Key::Enter,
        (b'u', b"13;2") => Key::Newline,
        (b'u', _) => Key::Esc, // other keys in that form: not bound
        (b'I', _) => Key::Focus(true),
        (b'O', _) => Key::Focus(false),
        _ => Key::Esc,
    };
    Decoded::Key(key, used)
}

/// `ESC [ 200 ~ <payload> ESC [ 201 ~`: accumulate until the end marker.
#[cfg(unix)]
fn decode_paste(bytes: &[u8], start: usize) -> Decoded {
    const END: &[u8] = b"\x1b[201~";
    let hay = &bytes[start..];
    let Some(pos) = hay.windows(END.len()).position(|w| w == END) else {
        return Decoded::NeedMore;
    };
    let payload = String::from_utf8_lossy(&hay[..pos]).into_owned();
    Decoded::Key(Key::Paste(payload), start + pos + END.len())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    fn one(input: &[u8]) -> (Key, usize) {
        match decode_one(input) {
            Decoded::Key(k, n) => (k, n),
            Decoded::NeedMore => panic!("incomplete: {input:?}"),
        }
    }

    #[test]
    fn decodes_simple_keys_and_utf8() {
        assert_eq!(one(b"\r").0, Key::Enter);
        assert_eq!(one(b"\n").0, Key::Newline);
        assert_eq!(one(b"\x7f").0, Key::Backspace);
        assert_eq!(one(b"\x03").0, Key::Interrupt);
        assert_eq!(one(b"\x0b").0, Key::Ctrl('k'));
        assert_eq!(one("ñ".as_bytes()).0, Key::Char('ñ'));
        assert_eq!(one("😀".as_bytes()).0, Key::Char('😀'));
        assert_eq!(one("😀".as_bytes()).1, 4);
    }

    #[test]
    fn decodes_csi_sequences_and_split_reads() {
        assert_eq!(one(b"\x1b[A").0, Key::Up);
        assert_eq!(one(b"\x1b[3~").0, Key::Delete);
        assert_eq!(one(b"\x1b[1;5C").0, Key::WordRight);
        assert_eq!(one(b"\x1b[H").0, Key::Home);
        assert_eq!(one(b"\x1b[13;2u").0, Key::Newline);
        assert_eq!(one(b"\x1b[I").0, Key::Focus(true));
        assert_eq!(one(b"\x1b[O").0, Key::Focus(false));
        assert_eq!(one(b"\x1b\r").0, Key::Newline);
        assert!(matches!(decode_one(b"\x1b["), Decoded::NeedMore));
        assert!(matches!(decode_one(b"\xc3"), Decoded::NeedMore));
    }

    #[test]
    fn decodes_bracketed_paste_with_newlines() {
        let (key, used) = one(b"\x1b[200~line one\nline two\x1b[201~");
        assert_eq!(key, Key::Paste("line one\nline two".into()));
        assert_eq!(used, 29);
        assert!(matches!(
            decode_one(b"\x1b[200~unterminated"),
            Decoded::NeedMore
        ));
    }
}
