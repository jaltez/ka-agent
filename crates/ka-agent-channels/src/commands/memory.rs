//! Memory and history commands: remember, forget, memory, history.

use super::{CmdFuture, Ctx, Outcome};
use ka_agent_core::ids::{id_matches, short_id};

pub(super) fn run_remember<'a>(ctx: Ctx<'a>) -> CmdFuture<'a> {
    Box::pin(async move {
        let fact = ctx.args;
        if fact.is_empty() {
            Outcome::Reply("usage: /remember <fact>".into())
        } else {
            match ctx.runtime.remember(fact, ctx.source) {
                Ok(id) => Outcome::Reply(format!("remembered {id}")),
                Err(e) => Outcome::Reply(format!("! [memory] {e}")),
            }
        }
    })
}

pub(super) fn run_forget<'a>(ctx: Ctx<'a>) -> CmdFuture<'a> {
    Box::pin(async move {
        let runtime = ctx.runtime;
        let prefix = ctx.args;
        if prefix.is_empty() {
            return Outcome::Reply("usage: /forget <id> — /memory lists them".into());
        }
        let entries = match runtime.memory_entries() {
            Ok(e) => e,
            Err(e) => return Outcome::Reply(format!("! [memory] {e}")),
        };
        let matches: Vec<_> = entries
            .iter()
            .filter(|e| id_matches(&e.id, prefix))
            .collect();
        match matches.as_slice() {
            [] => Outcome::Reply(format!("no memory matches {prefix}")),
            [one] => match runtime.forget(&one.id) {
                Ok(true) => Outcome::Reply("forgotten".into()),
                Ok(false) => Outcome::Reply("no such memory id".into()),
                Err(e) => Outcome::Reply(format!("! [memory] {e}")),
            },
            many => Outcome::Reply(format!(
                "ambiguous — matches: {}",
                many.iter()
                    .map(|e| short_id(&e.id))
                    .collect::<Vec<_>>()
                    .join(", ")
            )),
        }
    })
}

pub(super) fn run_memory<'a>(ctx: Ctx<'a>) -> CmdFuture<'a> {
    Box::pin(async move {
        let runtime = ctx.runtime;
        let rest = ctx.args;
        let reply = match rest {
            "log" => match runtime.memory_log(10) {
                Ok(rows) if rows.is_empty() => "no memory commits yet".to_string(),
                Ok(rows) => {
                    let mut out = String::from("recent memory changes (newest first):\n");
                    for (sha, date, subject) in rows {
                        out.push_str(&format!("  {sha} {date} {subject}\n"));
                    }
                    out.trim_end().to_string()
                }
                Err(e) => format!("git not installed — memory history is off ({e})"),
            },
            "undo" => match runtime.memory_undo() {
                Ok(subject) => format!("undone: {subject}"),
                Err(e) => format!("! [memory] {e}"),
            },
            "files" => match runtime.memory_report() {
                Ok(report) => {
                    let files: Vec<String> = report
                        .files
                        .iter()
                        .map(|f| format!("{} {}B", f.name, f.bytes))
                        .collect();
                    if files.is_empty() {
                        "(empty)".to_string()
                    } else {
                        files.join("\n")
                    }
                }
                Err(e) => format!("! [memory] {e}"),
            },
            "review" => {
                return Outcome::Reply(match runtime.pending_candidates() {
                    Ok(rows) if rows.is_empty() => "no pending candidates".to_string(),
                    Ok(rows) => rows
                        .into_iter()
                        .map(|c| format!("[{}] ({}) {}", short_id(c.id.as_str()), c.source, c.text))
                        .collect::<Vec<_>>()
                        .join("\n"),
                    Err(e) => format!("! [memory] {e}"),
                });
            }
            "" => match runtime.memory_entries() {
                Ok(entries) if entries.is_empty() => {
                    "nothing remembered yet — /remember <fact>".to_string()
                }
                Ok(entries) => {
                    let budget = runtime.memory_report().map(|r| r.budget_bytes).unwrap_or(0);
                    let total = runtime.memory_report().map(|r| r.total_bytes).unwrap_or(0);
                    let mut lines: Vec<String> = entries
                        .iter()
                        .take(15)
                        .map(|e| {
                            let text: String = e.text.chars().take(80).collect();
                            format!("[{}] {}", short_id(&e.id), text)
                        })
                        .collect();
                    lines.push(format!(
                        "{total}B of {budget}B used · /forget <id> · /memory review"
                    ));
                    lines.join("\n")
                }
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
                    _ => "usage: /memory [files|review|accept <id>|reject <id>]".to_string(),
                }
            }
        };
        Outcome::Reply(reply)
    })
}

pub(super) fn run_history<'a>(ctx: Ctx<'a>) -> CmdFuture<'a> {
    Box::pin(async move {
        let runtime = ctx.runtime;
        let rest = ctx.args;
        let reply = if let Some(query) = rest.strip_prefix("show ") {
            match runtime.history_show(query.trim()) {
                Ok(Some((inbound, answer))) => format!("> {inbound}\n{answer}"),
                Ok(None) => "no such turn".to_string(),
                Err(e) => format!("! [history] {e}"),
            }
        } else if rest.is_empty() {
            match runtime.recent_turns(5) {
                Ok(turns) if turns.is_empty() => "no history yet — say something first".into(),
                Ok(turns) => {
                    let now = runtime.now_zoned();
                    let take = |s: &str, n: usize| -> String { s.chars().take(n).collect() };
                    turns
                        .iter()
                        .map(|(t, answer)| {
                            let at = jiff::Timestamp::from_millisecond(t.created_ms)
                                .map(|ts| ts.to_zoned(now.time_zone().clone()))
                                .unwrap_or_else(|_| now.clone());
                            format!(
                                "[{}] {:02}:{:02} you: {} → {}",
                                short_id(t.id.as_str()),
                                at.hour(),
                                at.minute(),
                                take(&t.inbound_text, 60),
                                take(answer, 60)
                            )
                        })
                        .collect::<Vec<_>>()
                        .join("\n")
                }
                Err(e) => format!("! [history] {e}"),
            }
        } else {
            match runtime.history_search(rest, 5) {
                Ok(hits) if hits.is_empty() => format!("no history matches {rest:?}"),
                Ok(hits) => {
                    let now = runtime.now_zoned();
                    hits.into_iter()
                        .map(|h| {
                            let date = jiff::Timestamp::from_millisecond(h.at_ms)
                                .map(|ts| ts.to_zoned(now.time_zone().clone()))
                                .unwrap_or_else(|_| now.clone());
                            format!(
                                "[{}] {} · {:04}-{:02}-{:02}",
                                short_id(&h.turn),
                                h.snippet,
                                date.year(),
                                date.month() as u8,
                                date.day()
                            )
                        })
                        .collect::<Vec<_>>()
                        .join("\n")
                }
                Err(e) => format!("! [history] {e}"),
            }
        };
        Outcome::Reply(reply)
    })
}
