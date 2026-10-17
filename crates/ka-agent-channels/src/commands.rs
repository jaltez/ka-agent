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
                    /history <query> · /history show <turn_id> · /remind <Nd> <text>\n\
                    /cron [list] · /cron add (in|every) <Nd> <text> · /cron rm <id>\n\
                    /calendar add <url-or-path> · /calendar today · /y /n";

fn short_id(id: &str, n: usize) -> String {
    id.chars().take(n).collect()
}

/// `YYYY-MM-DD HH:MMZ` for timestamps in replies (dependency-free civil
/// conversion; Howard Hinnant's days-from-epoch inverse).
pub fn utc_compact(ms: i64) -> String {
    let secs = ms.div_euclid(1000);
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (h, m) = (rem / 3600, (rem % 3600) / 60);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if month <= 2 { y + 1 } else { y };
    format!("{year:04}-{month:02}-{d:02} {h:02}:{m:02}Z")
}

/// Render an interval compactly (`1h`, `30m`, `2d`, `45s`).
pub fn fmt_interval(ms: i64) -> String {
    for (unit, mult) in [("d", 86_400_000i64), ("h", 3_600_000), ("m", 60_000)] {
        if ms % mult == 0 {
            return format!("{}{unit}", ms / mult);
        }
    }
    format!("{}s", ms / 1000)
}

/// One line per live cron: `id every|once next-UTC text [paused]`.
pub fn cron_list_text(tasks: &[ka_agent_core::Task]) -> String {
    tasks
        .iter()
        .map(|t| {
            let spec = t
                .payload
                .as_deref()
                .and_then(ka_agent_core::CronSpec::from_payload);
            let cadence = match spec.as_ref().and_then(|s| s.recurrence_ms) {
                Some(ms) => format!("every {}", fmt_interval(ms)),
                None => "once".to_string(),
            };
            let next = t
                .scheduled_at_ms
                .map(utc_compact)
                .unwrap_or_else(|| "-".to_string());
            let paused = if t.status == ka_agent_core::TaskStatus::Interrupted {
                " [paused]"
            } else {
                ""
            };
            let text: String = t.title.chars().take(40).collect();
            format!(
                "{} {cadence} {next} {text}{paused}",
                short_id(t.id.as_str(), 8)
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// `/cron add (in|every) <Ns|Nm|Nh|Nd> <text>` → reply text. Shared by the
/// router and the console.
pub fn cron_add(runtime: &Runtime, args: &str) -> String {
    let mut parts = args.trim().splitn(3, ' ');
    let which = parts.next().unwrap_or("");
    let dur = parts.next().unwrap_or("");
    let text = parts.next().unwrap_or("").trim();
    if !matches!(which, "in" | "every") || text.is_empty() {
        return "usage: /cron add (in|every) <Ns|Nm|Nh|Nd> <text>".into();
    }
    let Some(ms) = ka_agent_core::parse_duration(dur) else {
        return format!("bad duration {dur:?}; use Ns, Nm, Nh or Nd");
    };
    let spec = ka_agent_core::CronSpec {
        text: text.to_string(),
        source: "operator".into(),
        recurrence_ms: which.eq("every").then_some(ms as i64),
        mode: ka_agent_core::CronMode::Agent,
    };
    match runtime.add_cron(spec, ka_agent_core::now_ms() + ms as i64) {
        Ok(id) => format!("cron {} scheduled", short_id(id.as_str(), 8)),
        Err(e) => format!("! [cron] {e}"),
    }
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
        "/cron" => {
            let rest = text.split_once(' ').map(|(_, r)| r.trim()).unwrap_or("");
            let reply = match rest {
                "" | "list" => match runtime.list_crons() {
                    Ok(rows) if rows.is_empty() => "no crons (use /cron add)".to_string(),
                    Ok(rows) => cron_list_text(&rows),
                    Err(e) => format!("! [cron] {e}"),
                },
                other => match other.split_once(' ') {
                    Some(("add", args)) => cron_add(runtime, args),
                    Some(("rm", id)) => match runtime.cancel_cron(id.trim()) {
                        Ok(true) => "cron removed".to_string(),
                        Ok(false) => "no such cron".to_string(),
                        Err(e) => format!("! [cron] {e}"),
                    },
                    _ => "usage: /cron [list] · /cron add (in|every) <Ns|Nm|Nh|Nd> <text> · /cron rm <id>"
                        .to_string(),
                },
            };
            Outcome::Reply(reply)
        }
        "/calendar" => {
            let rest = text.split_once(' ').map(|(_, r)| r.trim()).unwrap_or("");
            let (verb, arg) = match rest.split_once(' ') {
                Some((v, a)) => (v, a.trim()),
                None => (rest, ""),
            };
            let reply = match verb {
                "add" if !arg.is_empty() => {
                    let spec = ka_agent_core::CronSpec {
                        text: arg.to_string(),
                        source: "operator".into(),
                        recurrence_ms: Some(86_400_000),
                        mode: ka_agent_core::CronMode::NoAgent("calendar".into()),
                    };
                    match runtime.add_cron(spec, ka_agent_runtime::next_daily_utc(8)) {
                        Ok(_) => "calendar digest daily at 08:00 UTC".to_string(),
                        Err(e) => format!("! [calendar] {e}"),
                    }
                }
                "today" => {
                    let configured = runtime.list_crons().ok().and_then(|rows| {
                        rows.iter().find_map(|t| {
                            ka_agent_core::CronSpec::from_payload(t.payload.as_deref()?).filter(
                                |s| s.mode == ka_agent_core::CronMode::NoAgent("calendar".into()),
                            )
                        })
                    });
                    match configured {
                        Some(spec) => match runtime.run_no_agent("calendar", &spec.text) {
                            Ok(d) => d,
                            Err(e) => format!("! [calendar] {e}"),
                        },
                        None => {
                            "no calendar configured (use /calendar add <url-or-path>)".to_string()
                        }
                    }
                }
                _ => "usage: /calendar add <url-or-path> · /calendar today".to_string(),
            };
            Outcome::Reply(reply)
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
