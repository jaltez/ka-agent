//! Line-oriented prompts for the setup wizard and interactive CLIs: read
//! from stdin (terminal or pipe — scripts pipe the answers), secrets hidden
//! only when stdin is a terminal.

use std::io::{BufRead, IsTerminal, Write};

#[derive(Debug, thiserror::Error)]
pub enum PromptError {
    #[error("input closed")]
    Closed,
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

fn read_line() -> Result<String, PromptError> {
    let stdin = std::io::stdin();
    let mut line = String::new();
    let n = stdin.lock().read_line(&mut line)?;
    if n == 0 {
        return Err(PromptError::Closed);
    }
    Ok(line.trim().to_string())
}

fn flush_stdout() {
    let _ = std::io::stdout().flush();
}

/// Numbered menu. `Enter` picks `default`; anything unparsable re-asks.
/// Returns the chosen index into `options`.
pub fn choice(title: &str, options: &[String], default: usize) -> Result<usize, PromptError> {
    loop {
        if !title.is_empty() {
            println!("{title}");
        }
        for (i, o) in options.iter().enumerate() {
            let mark = if i == default { " (default)" } else { "" };
            println!("  {}) {o}{mark}", i + 1);
        }
        print!("Choose [{}]: ", default + 1);
        flush_stdout();
        let line = read_line()?;
        if line.is_empty() {
            return Ok(default);
        }
        match line.parse::<usize>() {
            Ok(n) if (1..=options.len()).contains(&n) => return Ok(n - 1),
            _ => continue,
        }
    }
}

/// Yes/no question; `Enter` picks `default`.
pub fn yes_no(question: &str, default: bool) -> Result<bool, PromptError> {
    loop {
        let hint = if default { "[Y/n]" } else { "[y/N]" };
        print!("{question} {hint}: ");
        flush_stdout();
        match read_line()?.to_lowercase().as_str() {
            "" => return Ok(default),
            "y" | "yes" => return Ok(true),
            "n" | "no" => return Ok(false),
            _ => continue,
        }
    }
}

/// Free text with an optional default shown in brackets.
pub fn text(question: &str, default: Option<&str>) -> Result<String, PromptError> {
    loop {
        match default {
            Some(d) => print!("{question} [{d}]: "),
            None => print!("{question}: "),
        }
        flush_stdout();
        let line = read_line()?;
        match (line.is_empty(), default) {
            (true, Some(d)) => return Ok(d.to_string()),
            (true, None) => continue,
            (false, _) => return Ok(line),
        }
    }
}

/// A secret: echoed off on a terminal (`rpassword`), a plain line
/// otherwise (piped scripts). Re-asks on empty input from a terminal;
/// a piped empty line is an error.
pub fn secret(question: &str) -> Result<String, PromptError> {
    loop {
        if std::io::stdin().is_terminal() {
            let value = rpassword::prompt_password(format!("{question}: "))?;
            let value = value.trim().to_string();
            if value.is_empty() {
                continue;
            }
            return Ok(value);
        }
        print!("{question} (input not hidden; stdin is not a terminal): ");
        flush_stdout();
        let line = read_line()?;
        if line.is_empty() {
            return Err(PromptError::Closed);
        }
        return Ok(line);
    }
}
