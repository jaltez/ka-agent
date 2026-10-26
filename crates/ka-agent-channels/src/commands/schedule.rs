//! Scheduling commands: remind, reminders, cron, calendar — plus the shared
//! rendering helpers they use (`cadence`, `cron_list_text`, `cron_add`).

use super::{CmdFuture, Ctx, Outcome};
use ka_agent_core::ids::short_id;
use ka_agent_runtime::Runtime;

/// The cadence column of `/reminders` and `/cron list`.
fn cadence(spec: Option<&ka_agent_core::CronSpec>) -> String {
    match spec {
        Some(s) if s.at_local.is_some() => {
            format!(
                "↻ {} {}",
                s.day_rule().describe(),
                s.at_local.clone().unwrap_or_default()
            )
        }
        Some(s) if s.recurrence_ms.is_some_and(|r| r > 0) => {
            format!(
                "↻ every {}",
                super::fmt_interval(s.recurrence_ms.unwrap_or(0))
            )
        }
        _ => "⏰".to_string(),
    }
}

/// One line per live task: `[id8] cadence next-fire title`.
pub fn cron_list_text(tasks: &[ka_agent_core::Task], now: &jiff::Zoned) -> String {
    tasks
        .iter()
        .map(|t| {
            let spec = t
                .payload
                .as_deref()
                .and_then(ka_agent_core::CronSpec::from_payload);
            let next = t
                .scheduled_at_ms
                .map(|at| ka_agent_core::when::describe(at, now))
                .unwrap_or_else(|| "-".to_string());
            let paused = if t.status == ka_agent_core::TaskStatus::Interrupted {
                " [paused]"
            } else {
                ""
            };
            let text: String = t.title.chars().take(40).collect();
            format!(
                "[{}] {} {} {}{}",
                short_id(t.id.as_str()),
                cadence(spec.as_ref()),
                next,
                text,
                paused
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// `/cron add …` → reply text. Shared by the router and the console.
pub fn cron_add(runtime: &Runtime, args: &str) -> String {
    cron_add_for(runtime, args, "operator", true)
}

/// Shared by `/cron add` and `ka-agent cron add`. `allow_gate` false is the
/// remote-source path (a gate is a shell command only the operator's own
/// surface may create).
pub fn cron_add_for(runtime: &Runtime, args: &str, source: &str, allow_gate: bool) -> String {
    let now = runtime.now_zoned();
    // `--if '<shell command>'` may appear anywhere; pull it out first so
    // the schedule words parse exactly as before.
    let (args, gate) = match split_gate(args) {
        Ok(v) => v,
        Err(e) => return e,
    };
    if gate.is_some() && !allow_gate {
        return "gates can only be added from the terminal, CLI or web panel".to_string();
    }
    let words: Vec<&str> = args.split_whitespace().collect();
    let (mut spec, first_at) = match ka_agent_core::CronSpec::from_words(&words, source, &now) {
        Ok(v) => v,
        Err(e) if e.starts_with("usage:") => return cron_add_usage(),
        Err(e) => return e,
    };
    spec.gate = gate;
    match runtime.add_cron(spec, first_at) {
        Ok(id) => format!(
            "cron {} scheduled — {}",
            short_id(id.as_str()),
            ka_agent_core::when::describe(first_at, &now)
        ),
        Err(e) => format!("! [cron] {e}"),
    }
}

/// Split a `--if '<command>'` flag out of the cron arguments.
fn split_gate(args: &str) -> Result<(String, Option<String>), String> {
    let Some(pos) = args.find(" --if ") else {
        return Ok((args.to_string(), None));
    };
    let (head, tail) = args.split_at(pos);
    let tail = tail.trim_start();
    let tail = tail.strip_prefix("--if").unwrap_or(tail).trim_start();
    // Quoted or bare: the command runs to the end (it is the last flag
    // before the text in the documented form).
    let (command, rest) = if let Some(q) = tail.chars().next().filter(|c| *c == '\'' || *c == '"') {
        match tail[1..].find(q) {
            Some(end) => (tail[1..1 + end].to_string(), &tail[2 + end..]),
            None => return Err("unterminated --if quote".into()),
        }
    } else {
        match tail.split_once(' ') {
            Some((cmd, rest)) => (cmd.to_string(), rest),
            None => (tail.to_string(), ""),
        }
    };
    if command.trim().is_empty() {
        return Err("--if needs a shell command".into());
    }
    Ok((
        format!("{head} {}", rest.trim()).trim().to_string(),
        Some(command),
    ))
}

fn cron_add_usage() -> String {
    "usage: /cron add in <duration> <text>\n      /cron add every <duration> <text>\n      /cron add every day at 08:00 <text>\n      /cron add every weekday at 9 <text>\n      /cron add every mon,wed,fri at 18:00 <text>\n      /cron add every month on the 1st at 9 <text>".into()
}

/// Cancel a reminder or cron by prefix; shared by `/remind rm` and `/cron rm`.
async fn cancel_by_prefix(runtime: &Runtime, prefix: &str) -> Outcome {
    match runtime.cancel_scheduled(prefix.trim()) {
        Ok(ka_agent_runtime::CancelOutcome::Cancelled(title)) => {
            Outcome::Reply(format!("cancelled: {title}"))
        }
        Ok(ka_agent_runtime::CancelOutcome::NotFound) => {
            Outcome::Reply(format!("no scheduled task matches {prefix:?}"))
        }
        Ok(ka_agent_runtime::CancelOutcome::Ambiguous(ids)) => {
            Outcome::Reply(format!("ambiguous — matches: {}", ids.join(", ")))
        }
        Err(e) => Outcome::Reply(format!("! [cancel] {e}")),
    }
}

pub(super) fn run_remind<'a>(ctx: Ctx<'a>) -> CmdFuture<'a> {
    Box::pin(async move {
        let runtime = ctx.runtime;
        let rest = ctx.args;
        if rest.is_empty() {
            return Outcome::Reply(remind_usage());
        }
        let words: Vec<&str> = rest.split_whitespace().collect();
        if words.first().is_some_and(|w| *w == "rm") {
            let prefix = words.get(1).copied().unwrap_or("").to_string();
            if prefix.is_empty() {
                return Outcome::Reply("usage: /remind rm <id>".into());
            }
            return cancel_by_prefix(runtime, &prefix).await;
        }
        // Leading `in` is grammar ("in 2h30m"), not part of the text.
        let now = runtime.now_zoned();
        let when = match ka_agent_core::when::parse_when(&words, &now) {
            Some(w) => w,
            None => return Outcome::Reply(remind_usage()),
        };
        let text = words[when.consumed..].join(" ").trim().to_string();
        if text.is_empty() {
            return Outcome::Reply(remind_usage());
        }
        match runtime.add_reminder_at(when.at_ms, &text) {
            Ok(id) => Outcome::Reply(format!(
                "⏰ {} — {} · id {}",
                ka_agent_core::when::describe(when.at_ms, &now),
                text,
                short_id(id.as_str())
            )),
            Err(e) => Outcome::Reply(format!("! [remind] {e}")),
        }
    })
}

pub(super) fn run_reminders<'a>(ctx: Ctx<'a>) -> CmdFuture<'a> {
    Box::pin(async move {
        let runtime = ctx.runtime;
        match runtime.scheduled_tasks() {
            Ok(rows) if rows.is_empty() => {
                Outcome::Reply("no reminders or crons — /remind <when> <text> schedules one".into())
            }
            Ok(rows) => {
                let now = runtime.now_zoned();
                let lines: Vec<String> = rows
                    .iter()
                    .map(|t| {
                        let spec = t
                            .payload
                            .as_deref()
                            .and_then(ka_agent_core::CronSpec::from_payload);
                        let next = t
                            .scheduled_at_ms
                            .map(|at| ka_agent_core::when::describe(at, &now))
                            .unwrap_or_else(|| "-".to_string());
                        let text: String = t.title.chars().take(50).collect();
                        format!(
                            "[{}] {} {} {}",
                            short_id(t.id.as_str()),
                            cadence(spec.as_ref()),
                            next,
                            text
                        )
                    })
                    .collect();
                Outcome::Reply(lines.join("\n"))
            }
            Err(e) => Outcome::Reply(format!("! [reminders] {e}")),
        }
    })
}

pub(super) fn run_cron<'a>(ctx: Ctx<'a>) -> CmdFuture<'a> {
    Box::pin(async move {
        let runtime = ctx.runtime;
        let rest = ctx.args;
        let reply = match rest {
            "" | "list" => match runtime.list_crons() {
                Ok(rows) if rows.is_empty() => "no crons (use /cron add)".to_string(),
                Ok(rows) => cron_list_text(&rows, &runtime.now_zoned()),
                Err(e) => format!("! [cron] {e}"),
            },
            other => match other.split_once(' ') {
                Some(("add", args)) => {
                    let local = ka_agent_core::is_local_source(ctx.source);
                    cron_add_for(runtime, args, "operator", local)
                }
                Some(("rm", id)) => {
                    return cancel_by_prefix(runtime, id).await;
                }
                _ => cron_add_usage(),
            },
        };
        Outcome::Reply(reply)
    })
}

pub(super) fn run_calendar<'a>(ctx: Ctx<'a>) -> CmdFuture<'a> {
    Box::pin(async move { calendar_command(ctx.runtime, ctx.args).await })
}

fn remind_usage() -> String {
    "usage: /remind <when> <text> — when: in 2h30m · at 18:30 · tomorrow 9am · fri 17:00 · 2026-11-03 10:00".into()
}

/// `/calendar add <src>`: an anchored daily digest at the configured local
/// time; `/calendar today`: render now.
async fn calendar_command(runtime: &Runtime, rest: &str) -> Outcome {
    let (verb, arg) = match rest.split_once(' ') {
        Some((v, a)) => (v, a.trim()),
        None => (rest, ""),
    };
    let digest_time = runtime.config().calendar.digest_time.clone();
    let tz_name = runtime
        .config()
        .time_zone()
        .iana_name()
        .unwrap_or("UTC")
        .to_string();
    match verb {
        "add" if !arg.is_empty() => {
            // HTTP sources need an explicit grant; create nothing without it.
            if arg.starts_with("http://") || arg.starts_with("https://") {
                let host = url::Url::parse(arg)
                    .ok()
                    .and_then(|u| u.host_str().map(str::to_string))
                    .unwrap_or_default();
                if host.is_empty() {
                    return Outcome::Reply(format!("bad url {arg:?}"));
                }
                let scope = format!("http:get:{host}");
                let granted = crate::grants::Grants::load(runtime.store())
                    .map(|g| g.check(&scope))
                    .unwrap_or(false);
                if !granted {
                    return Outcome::Reply(format!(
                        "needs a grant first — on your computer run: ka-agent grant add {scope}"
                    ));
                }
            }
            // One digest at a time: replace the previous calendar cron.
            if let Ok(rows) = runtime.list_crons() {
                for t in rows {
                    let is_calendar = t
                        .payload
                        .as_deref()
                        .and_then(ka_agent_core::CronSpec::from_payload)
                        .is_some_and(|s| {
                            s.mode == ka_agent_core::CronMode::NoAgent("calendar".into())
                        });
                    if is_calendar {
                        let _ = runtime.cancel_scheduled(t.id.as_str());
                    }
                }
            }
            let Some((hh, mm)) = ka_agent_core::when::parse_hhmm(&digest_time) else {
                return Outcome::Reply(format!(
                    "! [calendar] calendar.digest_time {digest_time:?} must be HH:MM (24h)"
                ));
            };
            let now = runtime.now_zoned();
            let spec = ka_agent_core::CronSpec {
                text: arg.to_string(),
                source: "operator".into(),
                recurrence_ms: Some(86_400_000),
                mode: ka_agent_core::CronMode::NoAgent("calendar".into()),
                at_local: Some(digest_time.clone()),
                weekdays: Vec::new(),
                month_day: None,
                gate: None,
            };
            let first_at =
                ka_agent_core::when::next_local(hh, mm, &ka_agent_core::when::DayRule::Daily, &now);
            match runtime.add_cron(spec, first_at) {
                Ok(_) => Outcome::Reply(format!(
                    "calendar digest daily at {digest_time} ({tz_name})"
                )),
                Err(e) => Outcome::Reply(format!("! [calendar] {e}")),
            }
        }
        "today" => {
            let configured = runtime.list_crons().ok().and_then(|rows| {
                rows.iter().find_map(|t| {
                    ka_agent_core::CronSpec::from_payload(t.payload.as_deref()?)
                        .filter(|s| s.mode == ka_agent_core::CronMode::NoAgent("calendar".into()))
                })
            });
            match configured {
                Some(spec) => match runtime.run_no_agent("calendar", &spec.text) {
                    Ok(d) => Outcome::Reply(d),
                    Err(e) => Outcome::Reply(format!("! [calendar] {e}")),
                },
                None => Outcome::Reply(
                    "no calendar configured (use /calendar add <url-or-path>)".into(),
                ),
            }
        }
        _ => Outcome::Reply("usage: /calendar add <url-or-path> · /calendar today".into()),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[test]
    fn cadence_renders_anchored_and_intervals() {
        let anchored = ka_agent_core::CronSpec {
            text: "x".into(),
            source: "t".into(),
            recurrence_ms: Some(86_400_000),
            mode: ka_agent_core::CronMode::Agent,
            at_local: Some("08:00".into()),
            weekdays: Vec::new(),
            month_day: None,
            gate: None,
        };
        let weekly = ka_agent_core::CronSpec {
            weekdays: vec![0],
            month_day: None,
            gate: None,
            at_local: Some("09:00".into()),
            ..anchored.clone()
        };
        assert_eq!(cadence(Some(&anchored)), "↻ daily 08:00");
        assert_eq!(cadence(Some(&weekly)), "↻ mon 09:00");
        let interval = ka_agent_core::CronSpec {
            recurrence_ms: Some(3_600_000),
            at_local: None,
            ..anchored
        };
        assert_eq!(cadence(Some(&interval)), "↻ every 1h");
    }
}
