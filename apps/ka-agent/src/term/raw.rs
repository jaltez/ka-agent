//! Raw terminal mode (Unix): a guard that puts stdin in raw mode with
//! output post-processing kept on, enables bracketed paste + focus
//! reports, and restores everything on drop, panic and common signals.
//! Windows reaches this module in phase 4 (a line-mode fallback exists
//! until then).

use std::sync::OnceLock;

/// The original termios, captured once so a panic hook or a signal handler
/// can restore it even after a `Terminal` was forgotten.
static ORIGINAL: OnceLock<rustix::termios::Termios> = OnceLock::new();

#[cfg(unix)]
pub fn is_tty() -> bool {
    use std::io::IsTerminal as _;
    std::io::stdin().is_terminal() && std::io::stdout().is_terminal()
}

#[cfg(not(unix))]
pub fn is_tty() -> bool {
    false
}

/// Enter raw mode. Returns `None` when the platform/terminal cannot (the
/// caller falls back to the line-based path).
#[cfg(unix)]
pub fn enter() -> Option<RawMode> {
    use rustix::termios::{OptionalActions, OutputModes, tcgetattr, tcsetattr};
    let stdin = rustix::stdio::stdin();
    let original = tcgetattr(stdin).ok()?;
    let _ = ORIGINAL.set(original.clone());
    let mut raw = original.clone();
    raw.make_raw();
    // Keep output post-processing: without OPOST, `\n` does not return the
    // carriage, and every line would stair-step.
    raw.output_modes |= OutputModes::OPOST;
    tcsetattr(stdin, OptionalActions::Drain, &raw).ok()?;
    // Bracketed paste + focus reports: a pasted block arrives as one
    // event, and the status line knows whether the operator is looking.
    print!("\x1b[?2004h\x1b[?1004h");
    flush();
    install_panic_hook();
    Some(RawMode { _priv: () })
}

#[cfg(not(unix))]
pub fn enter() -> Option<RawMode> {
    None
}

fn flush() {
    use std::io::Write as _;
    let _ = std::io::stdout().flush();
}

fn install_panic_hook() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let default = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            restore();
            default(info);
        }));
    });
}

/// Restore the terminal (also called from the panic hook).
pub fn restore() {
    #[cfg(unix)]
    {
        use rustix::termios::{OptionalActions, tcsetattr};
        if let Some(original) = ORIGINAL.get() {
            let _ = tcsetattr(rustix::stdio::stdin(), OptionalActions::Flush, original);
        }
        print!("\x1b[?1004l\x1b[?2004l");
        flush();
    }
}

/// The raw-mode guard: dropping it restores the terminal.
pub struct RawMode {
    _priv: (),
}

impl Drop for RawMode {
    fn drop(&mut self) {
        restore();
    }
}

/// Suspend raw mode around an external editor run and re-enter after.
pub struct Suspended;

impl Suspended {
    pub fn begin() -> Self {
        restore();
        Self
    }

    pub fn end(self) {
        #[cfg(unix)]
        {
            use rustix::termios::{OptionalActions, OutputModes, tcsetattr};
            if let Some(original) = ORIGINAL.get() {
                let mut raw = original.clone();
                raw.make_raw();
                raw.output_modes |= OutputModes::OPOST;
                let _ = tcsetattr(rustix::stdio::stdin(), OptionalActions::Drain, &raw);
                // Re-enable the private modes the editor reset.
                print!("\x1b[?2004h\x1b[?1004h");
                flush();
            }
        }
    }
}
