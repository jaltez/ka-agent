//! The shared command router (SEED.md §3.6): every channel parses the same
//! `/commands` and gets the same replies; only the transport differs.
//! Free text is returned as [`Outcome::NotACommand`] for the channel to
//! hand to the runtime as a message.

use ka_agent_runtime::Runtime;

/// What a channel should do after routing one inbound line.
pub enum Outcome {
    /// Send this text back to the sender.
    Reply(String),
    /// Handled, nothing to send (e.g. `/stop` succeeded quietly).
    Silent,
    /// Not a command: the channel forwards it to `Runtime::handle_message`.
    NotACommand,
}

const HELP: &str = "commands: /help /status /new /stop /resume\n\
                    /remember <fact> · /forget <id> · /memory [review|accept <id>|reject <id>]\n\
                    /history <query> · /history show <turn_id> · /remind <Nd> <text> · /y /n";

fn short_id(id: &str, n: usize) -> String {
    id.chars().take(n).collect()
}

/// Route one inbound line. `source` is the channel identity
/// (`telegram:12345`, `matrix:@a:b`) used for provenance.
pub async fn route(runtime: &Runtime, source: &str, text: &str) -> Outcome {
    let Some(word) = text.split_whitespace().next() else {
        return Outcome::NotACommand;
    };
    if !word.starts_with('/') {
        return Outcome::NotACommand;
    }
    match word {
        "/start" | "/help" => Outcome::Reply(HELP.to_string()),
        "/remember" => {
            let fact = text.split_once(' ').map(|(_, f)| f.trim()).unwrap_or("");
            if fact.is_empty() {
                Outcome::Reply("usage: /remember <fact>".into())
            } else {
                match runtime.remember(fact, source) {
                    Ok(id) => Outcome::Reply(format!("remembered {id}")),
                    Err(e) => Outcome::Reply(format!("! [memory] {e}")),
                }
            }
        }
        "/forget" => {
            let Some((_, id)) = text.split_once(' ') else {
                return Outcome::Reply("usage: /forget <mem_id>".into());
            };
            match runtime.forget(id.trim()) {
                Ok(true) => Outcome::Reply("forgotten".into()),
                Ok(false) => Outcome::Reply("no such memory id".into()),
                Err(e) => Outcome::Reply(format!("! [memory] {e}")),
            }
        }
        "/memory" => {
            let rest = text.split_once(' ').map(|(_, r)| r.trim()).unwrap_or("");
            let reply = match rest {
                "" => match runtime.memory_report() {
                    Ok(report) => {
                        let files: Vec<String> = report
                            .files
                            .iter()
                            .map(|f| format!("{} {}B", f.name, f.bytes))
                            .collect();
                        format!(
                            "memory: {}B/{}B{}\n{}",
                            report.total_bytes,
                            report.budget_bytes,
                            if report.truncated { " (truncated)" } else { "" },
                            if files.is_empty() {
                                "(empty)".to_string()
                            } else {
                                files.join("\n")
                            }
                        )
                    }
                    Err(e) => format!("! [memory] {e}"),
                },
                "review" => match runtime.pending_candidates() {
                    Ok(rows) if rows.is_empty() => "no pending candidates".to_string(),
                    Ok(rows) => rows
                        .into_iter()
                        .map(|c| {
                            format!("{} ({}): {}", short_id(c.id.as_str(), 10), c.source, c.text)
                        })
                        .collect::<Vec<_>>()
                        .join("\n"),
                    Err(e) => format!("! [memory] {e}"),
                },
                other => {
                    let (verb, id) = other.split_once(' ').unwrap_or((other, ""));
                    match verb {
                        "accept" => match runtime.accept_candidate(id.trim()) {
                            Ok(true) => "accepted into stable memory".to_string(),
                            Ok(false) => "no such pending candidate".to_string(),
                            Err(e) => format!("! [memory] {e}"),
                        },
                        "reject" => match runtime.reject_candidate(id.trim()) {
                            Ok(true) => "rejected".to_string(),
                            Ok(false) => "no such pending candidate".to_string(),
                            Err(e) => format!("! [memory] {e}"),
                        },
                        _ => "usage: /memory [review|accept <id>|reject <id>]".to_string(),
                    }
                }
            };
            Outcome::Reply(reply)
        }
        "/remind" => {
            let rest = text.split_once(' ').map(|(_, r)| r.trim()).unwrap_or("");
            match runtime.add_reminder(rest) {
                Ok(Some(id)) => Outcome::Reply(format!("reminder {id} scheduled")),
                Ok(None) => Outcome::Reply("usage: /remind <Ns|Nm|Nh|Nd> <text>".into()),
                Err(e) => Outcome::Reply(format!("! [remind] {e}")),
            }
        }
        "/resume" => match runtime.resume() {
            Ok(true) => Outcome::Reply("resumed".into()),
            Ok(false) => Outcome::Reply("nothing to resume (not paused)".into()),
            Err(e) => Outcome::Reply(format!("! [resume] {e}")),
        },
        "/skills" => {
            Outcome::Reply("skills live in <data>/engine-home/.config/ka/skills/ (SKILL.md)".into())
        }
        "/history" => {
            let rest = text.split_once(' ').map(|(_, r)| r.trim()).unwrap_or("");
            let reply = if let Some(query) = rest.strip_prefix("show ") {
                match runtime.history_show(query.trim()) {
                    Ok(Some((inbound, answer))) => format!("> {inbound}\n{answer}"),
                    Ok(None) => "no such turn".to_string(),
                    Err(e) => format!("! [history] {e}"),
                }
            } else if rest.is_empty() {
                "usage: /history <query> · /history show <turn_id>".to_string()
            } else {
                match runtime.history_search(rest, 5) {
                    Ok(hits) if hits.is_empty() => format!("no history matches {rest:?}"),
                    Ok(hits) => hits
                        .into_iter()
                        .map(|h| format!("[{}] {}", short_id(&h.turn, 8), h.snippet))
                        .collect::<Vec<_>>()
                        .join("\n"),
                    Err(e) => format!("! [history] {e}"),
                }
            };
            Outcome::Reply(reply)
        }
        "/status" => {
            let (status, strand, usd, tokens, crashes) = runtime.status();
            let session = short_id(runtime.session_id().as_str(), 8);
            let strand = strand
                .as_deref()
                .map(|s| short_id(s, 8))
                .unwrap_or_else(|| "new".to_string());
            Outcome::Reply(format!(
                "session {session} · {status:?}\nstrand: {strand}\nbudget: ${usd:.4} + {tokens} tokens · crashes: {crashes}"
            ))
        }
        "/new" => match runtime.new_session().await {
            Ok(id) => Outcome::Reply(format!("new session {}", short_id(id.as_str(), 8))),
            Err(e) => Outcome::Reply(format!("! [session] {e}")),
        },
        "/stop" => match runtime.stop_turn().await {
            Ok(()) => Outcome::Silent,
            Err(e) => Outcome::Reply(format!("! [stop] {e}")),
        },
        // Text approvals (the console and Matrix model; Telegram uses its
        // inline buttons but accepts these too).
        "/y" | "/n" => {
            use ka_agent_core::ApprovalDecision;
            let decision = if word == "/y" {
                ApprovalDecision::Allow
            } else {
                ApprovalDecision::Deny
            };
            match runtime.decide_latest(decision).await {
                Ok(true) => Outcome::Reply("decided".into()),
                Ok(false) => Outcome::Reply("no pending approval".into()),
                Err(e) => Outcome::Reply(format!("! [approval] {e}")),
            }
        }
        other => Outcome::Reply(format!("unknown command: {other}")),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[test]
    fn help_is_stable_text() {
        assert!(HELP.contains("/status"));
        assert!(HELP.contains("/y /n"));
    }

    #[test]
    fn short_id_truncates() {
        assert_eq!(short_id("abcdefgh", 3), "abc");
    }
}
