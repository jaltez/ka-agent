//! The shared command router (SEED.md §3.6): every channel parses the same
//! `/commands` and gets the same replies; only the transport differs.
//! Free text is returned as [`Outcome::NotACommand`] for the channel to
//! hand to the runtime as a message.

use ka_agent_core::ids::{id_matches, short_id};
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

/// One command, for `/help`, `/help <name>` and the Telegram command menu.
pub struct CommandSpec {
    pub name: &'static str,
    pub args: &'static str,
    pub summary: &'static str,
    pub example: &'static str,
    /// Listed in Telegram's bot-command menu (fits its 256-char limit).
    pub telegram: bool,
}

pub const COMMANDS: &[CommandSpec] = &[
    CommandSpec {
        name: "help",
        args: "[command]",
        summary: "list commands, or explain one",
        example: "/help remind",
        telegram: true,
    },
    CommandSpec {
        name: "status",
        args: "",
        summary: "session, model, spend, queue, next task",
        example: "/status",
        telegram: true,
    },
    CommandSpec {
        name: "new",
        args: "",
        summary: "start a fresh session (stops the current worker)",
        example: "/new",
        telegram: true,
    },
    CommandSpec {
        name: "stop",
        args: "",
        summary: "abort the running reply",
        example: "/stop",
        telegram: true,
    },
    CommandSpec {
        name: "resume",
        args: "",
        summary: "continue after a pause (budget or provider key)",
        example: "/resume",
        telegram: true,
    },
    CommandSpec {
        name: "model",
        args: "[provider/model]",
        summary: "show or switch the model",
        example: "/model anthropic/claude-sonnet-5",
        telegram: true,
    },
    CommandSpec {
        name: "remember",
        args: "<fact>",
        summary: "keep a durable fact",
        example: "/remember I am allergic to shellfish",
        telegram: true,
    },
    CommandSpec {
        name: "forget",
        args: "<id>",
        summary: "remove a remembered fact (see /memory)",
        example: "/forget a1b2c3d4",
        telegram: true,
    },
    CommandSpec {
        name: "memory",
        args: "[files|review]",
        summary: "what is remembered and its budget",
        example: "/memory",
        telegram: true,
    },
    CommandSpec {
        name: "history",
        args: "[query|show <id>]",
        summary: "past turns of this session",
        example: "/history volcano",
        telegram: true,
    },
    CommandSpec {
        name: "remind",
        args: "<when> <text>",
        summary: "remind me at a time (2h30m, 18:30, tomorrow 9am)",
        example: "/remind tomorrow 9am call mom",
        telegram: true,
    },
    CommandSpec {
        name: "reminders",
        args: "",
        summary: "upcoming reminders and crons",
        example: "/reminders",
        telegram: true,
    },
    CommandSpec {
        name: "cron",
        args: "[add|rm]",
        summary: "scheduled prompts (in 10m, every 1h, daily 08:00)",
        example: "/cron add every day at 08:00 summarize my inbox",
        telegram: true,
    },
    CommandSpec {
        name: "calendar",
        args: "add <src>|today",
        summary: "daily calendar digest",
        example: "/calendar add https://…/calendar.ics",
        telegram: true,
    },
    CommandSpec {
        name: "skills",
        args: "",
        summary: "enabled skills",
        example: "/skills",
        telegram: true,
    },
    CommandSpec {
        name: "skill",
        args: "<name> <text>",
        summary: "run one turn with a skill loaded",
        example: "/skill review this diff",
        telegram: true,
    },
    CommandSpec {
        name: "approvals",
        args: "",
        summary: "pending approvals",
        example: "/approvals",
        telegram: true,
    },
    CommandSpec {
        name: "y",
        args: "[id]",
        summary: "allow a pending approval",
        example: "/y",
        telegram: true,
    },
    CommandSpec {
        name: "n",
        args: "[id]",
        summary: "deny a pending approval",
        example: "/n",
        telegram: true,
    },
];

/// One line per command: `/name args — summary`.
pub fn help_text() -> String {
    let mut out = COMMANDS
        .iter()
        .map(|c| {
            if c.args.is_empty() {
                format!("/{} — {}", c.name, c.summary)
            } else {
                format!("/{} {} — {}", c.name, c.args, c.summary)
            }
        })
        .collect::<Vec<_>>()
        .join("\n");
    out.push_str(
        "\n\nTip: just talk to me — commands are optional. @file or @https://… adds context.",
    );
    out
}

/// `/help <name>`: usage, summary, example.
pub fn help_for(name: &str) -> Option<String> {
    let c = COMMANDS.iter().find(|c| c.name == name)?;
    Some(format!(
        "/{} {}\n{}\nexample: {}",
        c.name, c.args, c.summary, c.example
    ))
}

/// `2026-10-30 18:30` in `now`'s zone.
pub fn local_compact(ms: i64, now: &jiff::Zoned) -> String {
    let z = jiff::Timestamp::from_millisecond(ms)
        .map(|t| t.to_zoned(now.time_zone().clone()))
        .unwrap_or_else(|_| now.clone());
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}",
        z.year(),
        z.month() as u8,
        z.day(),
        z.hour(),
        z.minute()
    )
}

/// Render an interval compactly (`1h`, `30m`, `2d`, `45s`) — re-exported
/// from core so surfaces and the runtime share one formatter.
pub fn fmt_interval(ms: i64) -> String {
    ka_agent_core::fmt_interval(ms)
}

/// The cadence column of `/reminders` and `/cron list`.
fn cadence(spec: Option<&ka_agent_core::CronSpec>) -> String {
    match spec {
        Some(s) if s.at_local.is_some() => {
            let day = match s.weekday {
                Some(w) => weekday_name(w),
                None => "daily".to_string(),
            };
            format!("↻ {day} {}", s.at_local.clone().unwrap_or_default())
        }
        Some(s) if s.recurrence_ms.is_some_and(|r| r > 0) => {
            format!("↻ every {}", fmt_interval(s.recurrence_ms.unwrap_or(0)))
        }
        _ => "⏰".to_string(),
    }
}

fn weekday_name(w: u8) -> String {
    const NAMES: [&str; 7] = ["mon", "tue", "wed", "thu", "fri", "sat", "sun"];
    match jiff::civil::Weekday::from_monday_zero_offset(w as i8) {
        Ok(d) => NAMES[d.to_monday_zero_offset() as usize].to_string(),
        Err(_) => "daily".to_string(),
    }
}

fn weekday_from_name(s: &str) -> Option<u8> {
    let n = s.trim().to_lowercase();
    let full = [
        "monday",
        "tuesday",
        "wednesday",
        "thursday",
        "friday",
        "saturday",
        "sunday",
    ];
    for (i, d) in full.iter().enumerate() {
        if n == *d || n == d[..3.min(d.len())] {
            return Some(i as u8);
        }
    }
    None
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
    let now = runtime.now_zoned();
    let parts: Vec<&str> = args.split_whitespace().collect();
    if parts.len() < 3 {
        return cron_add_usage();
    }
    let first = parts[0];
    let anchored = |at_local: &str, weekday: Option<u8>, text: String| {
        let Some((hh, mm)) = ka_agent_core::when::parse_hhmm(at_local) else {
            return format!("bad time {at_local:?} — use HH:MM (24h), e.g. 08:30");
        };
        let spec = ka_agent_core::CronSpec {
            text,
            source: "operator".into(),
            recurrence_ms: Some(86_400_000),
            mode: ka_agent_core::CronMode::Agent,
            at_local: Some(at_local.to_string()),
            weekday,
        };
        let first_at = ka_agent_core::when::next_local(
            hh,
            mm,
            weekday.map(|w| {
                jiff::civil::Weekday::from_monday_zero_offset(w as i8)
                    .unwrap_or(jiff::civil::Weekday::Monday)
            }),
            &now,
        );
        match runtime.add_cron(spec, first_at) {
            Ok(id) => format!(
                "cron {} scheduled — {}",
                short_id(id.as_str()),
                ka_agent_core::when::describe(first_at, &now)
            ),
            Err(e) => format!("! [cron] {e}"),
        }
    };
    if first == "in" {
        let Some(ms) = ka_agent_core::parse_duration(parts[1]) else {
            return format!("bad duration {:?}; use Ns, Nm, Nh or Nd", parts[1]);
        };
        let text = parts[2..].join(" ");
        let spec = ka_agent_core::CronSpec {
            text,
            source: "operator".into(),
            recurrence_ms: None,
            mode: ka_agent_core::CronMode::Agent,
            at_local: None,
            weekday: None,
        };
        let at = ka_agent_core::now_ms() + ms as i64;
        return match runtime.add_cron(spec, at) {
            Ok(id) => format!(
                "cron {} scheduled — {}",
                short_id(id.as_str()),
                ka_agent_core::when::describe(at, &now)
            ),
            Err(e) => format!("! [cron] {e}"),
        };
    }
    if first == "every" {
        let second = parts[1];
        if second.eq_ignore_ascii_case("day") {
            // every day at HH:MM <text>  |  every day HH:MM <text>
            let (at, off) = if parts[2].eq_ignore_ascii_case("at") {
                (parts.get(3).copied().unwrap_or(""), 4)
            } else {
                (parts[2], 3)
            };
            let text = parts[off.min(parts.len())..].join(" ");
            if text.is_empty() {
                return cron_add_usage();
            }
            return anchored(at, None, text);
        }
        if let Some(w) = weekday_from_name(second) {
            let (at, off) = if parts[2].eq_ignore_ascii_case("at") {
                (parts.get(3).copied().unwrap_or(""), 4)
            } else {
                (parts[2], 3)
            };
            let text = parts[off.min(parts.len())..].join(" ");
            if text.is_empty() {
                return cron_add_usage();
            }
            return anchored(at, Some(w), text);
        }
        // every <duration> <text>: interval recurrence.
        let Some(ms) = ka_agent_core::parse_duration(second) else {
            return format!("bad duration {second:?}; use Ns, Nm, Nh or Nd");
        };
        let text = parts[2..].join(" ");
        if text.is_empty() {
            return cron_add_usage();
        }
        let spec = ka_agent_core::CronSpec {
            text,
            source: "operator".into(),
            recurrence_ms: Some(ms as i64),
            mode: ka_agent_core::CronMode::Agent,
            at_local: None,
            weekday: None,
        };
        let at = ka_agent_core::now_ms() + ms as i64;
        return match runtime.add_cron(spec, at) {
            Ok(id) => format!(
                "cron {} scheduled — {}",
                short_id(id.as_str()),
                ka_agent_core::when::describe(at, &now)
            ),
            Err(e) => format!("! [cron] {e}"),
        };
    }
    cron_add_usage()
}

fn cron_add_usage() -> String {
    "usage: /cron add in <duration> <text>\n      /cron add every <duration> <text>\n      /cron add every day at 08:00 <text>\n      /cron add every mon at 09:00 <text>".into()
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

/// The operator's answer to `/model` with no argument.
async fn model_status(runtime: &Runtime) -> Outcome {
    let current = runtime.config().model.clone();
    let key = match runtime.model_key_state() {
        Ok(Some((_, state))) => match state {
            ka_agent_runtime::ModelKeyState::Present => None,
            ka_agent_runtime::ModelKeyState::Missing(env) => {
                Some(format!("✗ {env} missing — ka-agent key set {env}"))
            }
            ka_agent_runtime::ModelKeyState::Keyless => None,
        },
        Ok(None) => None,
        Err(e) => return Outcome::Reply(format!("! [model] {e}")),
    };
    let mut lines = vec![match &current {
        Some(m) => format!("model: {m}"),
        None => "model: none — run `ka-agent setup`".to_string(),
    }];
    if let Some(k) = key {
        lines.push(k);
    }
    lines.push("switch: ka-agent model provider/model · list: ka-agent models".into());
    Outcome::Reply(lines.join("\n"))
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
    // `/status@my_bot`: Telegram appends the bot's name in groups.
    let name = word.split('@').next().unwrap_or(word);
    let rest_of = |cmd: &str| -> &str {
        text.strip_prefix(cmd)
            .map(str::trim_start)
            .unwrap_or("")
            .trim()
    };
    match name {
        "/start" => Outcome::Reply(help_text()),
        "/help" => {
            let topic = rest_of("/help").trim_start_matches('/');
            if topic.is_empty() {
                Outcome::Reply(help_text())
            } else {
                match help_for(topic) {
                    Some(h) => Outcome::Reply(h),
                    None => Outcome::Reply(format!("unknown command /{topic} — /help lists them")),
                }
            }
        }
        "/status" => match runtime.status_report() {
            Ok(report) => Outcome::Reply(report.render(&runtime.now_zoned())),
            Err(e) => Outcome::Reply(format!("! [status] {e}")),
        },
        "/new" => match runtime.new_session().await {
            Ok(id) => Outcome::Reply(format!("new session {}", short_id(id.as_str()))),
            Err(e) => Outcome::Reply(format!("! [session] {e}")),
        },
        "/stop" => match runtime.stop_turn().await {
            Ok(()) => Outcome::Silent,
            Err(e) => Outcome::Reply(format!("! [stop] {e}")),
        },
        "/resume" => match runtime.resume() {
            Ok(true) => Outcome::Reply("resumed".into()),
            Ok(false) => Outcome::Reply("nothing to resume (not paused)".into()),
            Err(e) => Outcome::Reply(format!("! [resume] {e}")),
        },
        "/model" => {
            let rest = rest_of("/model");
            if rest.is_empty() {
                model_status(runtime).await
            } else {
                match runtime.set_model(rest).await {
                    Ok(id) => {
                        let mut reply = format!("model → {id} (next message uses it)");
                        if let Ok(Some((_, ka_agent_runtime::ModelKeyState::Missing(env)))) =
                            runtime.model_key_state()
                        {
                            reply.push_str(&format!("\n✗ {env} missing — ka-agent key set {env}"));
                        }
                        Outcome::Reply(reply)
                    }
                    Err(e) => Outcome::Reply(format!("! [model] {e}")),
                }
            }
        }
        "/remember" => {
            let fact = rest_of("/remember");
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
            let prefix = rest_of("/forget");
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
        }
        "/memory" => {
            let rest = rest_of("/memory");
            let reply = match rest {
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
                            .map(|c| format!("{} ({}): {}", c.id.as_str(), c.source, c.text))
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
        }
        "/remind" => {
            let rest = rest_of("/remind");
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
        }
        "/reminders" => match runtime.scheduled_tasks() {
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
        },
        "/cron" => {
            let rest = rest_of("/cron");
            let reply = match rest {
                "" | "list" => match runtime.list_crons() {
                    Ok(rows) if rows.is_empty() => "no crons (use /cron add)".to_string(),
                    Ok(rows) => cron_list_text(&rows, &runtime.now_zoned()),
                    Err(e) => format!("! [cron] {e}"),
                },
                other => match other.split_once(' ') {
                    Some(("add", args)) => cron_add(runtime, args),
                    Some(("rm", id)) => {
                        return cancel_by_prefix(runtime, id).await;
                    }
                    _ => cron_add_usage(),
                },
            };
            Outcome::Reply(reply)
        }
        "/calendar" => calendar_command(runtime, rest_of("/calendar")).await,
        "/approvals" => match runtime.pending_approval_cards() {
            Ok(cards) if cards.is_empty() => Outcome::Reply("no pending approvals".into()),
            Ok(cards) => Outcome::Reply(
                cards
                    .iter()
                    .map(|c| {
                        let question: String = c.question.lines().next().unwrap_or("").into();
                        format!(
                            "[{}] {} — expires in {}",
                            short_id(&c.approval_id),
                            question,
                            fmt_interval(c.expires_at_ms - ka_agent_core::now_ms())
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("\n"),
            ),
            Err(e) => Outcome::Reply(format!("! [approvals] {e}")),
        },
        "/skills" => {
            let enabled = runtime.skills_enabled();
            let reply = if enabled.is_empty() {
                "no skills enabled — `ka-agent skills enable <name>` provisions one".to_string()
            } else {
                enabled
                    .into_iter()
                    .map(|s| {
                        if s.description.is_empty() {
                            s.name
                        } else {
                            format!("{} — {}", s.name, s.description)
                        }
                    })
                    .collect::<Vec<_>>()
                    .join("\n")
            };
            Outcome::Reply(reply)
        }
        "/skill" => {
            let rest = rest_of("/skill");
            match rest.split_once(' ') {
                Some((name, prompt)) if !name.is_empty() && !prompt.is_empty() => {
                    match runtime.handle_skill(name, prompt).await {
                        // The turn itself streams back through the surface.
                        Ok(()) => Outcome::Silent,
                        Err(e) => Outcome::Reply(format!("! [skill] {e}")),
                    }
                }
                _ => Outcome::Reply("usage: /skill <name> <text>".into()),
            }
        }
        "/history" => {
            let rest = rest_of("/history");
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
        }
        "/y" | "/n" => {
            use ka_agent_core::ApprovalDecision;
            let decision = if name == "/y" {
                ApprovalDecision::Allow
            } else {
                ApprovalDecision::Deny
            };
            let prefix = rest_of(name);
            if prefix.is_empty() {
                match runtime.decide_latest(decision).await {
                    Ok(true) => Outcome::Reply("decided".into()),
                    Ok(false) => Outcome::Reply("no pending approval".into()),
                    Err(e) => Outcome::Reply(format!("! [approval] {e}")),
                }
            } else {
                let cards = match runtime.pending_approval_cards() {
                    Ok(c) => c,
                    Err(e) => return Outcome::Reply(format!("! [approval] {e}")),
                };
                let matches: Vec<_> = cards
                    .iter()
                    .filter(|c| id_matches(&c.approval_id, prefix))
                    .collect();
                match matches.as_slice() {
                    [one] => match runtime.decide(&one.approval_id, decision).await {
                        Ok(()) => Outcome::Reply("decided".into()),
                        Err(e) => Outcome::Reply(format!("! [approval] {e}")),
                    },
                    [] => Outcome::Reply(format!("no pending approval matches {prefix}")),
                    _ => Outcome::Reply("ambiguous approval id — /approvals lists them".into()),
                }
            }
        }
        other => Outcome::Reply(format!("unknown command {other} — /help lists them")),
    }
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
                weekday: None,
            };
            let first_at = ka_agent_core::when::next_local(hh, mm, None, &now);
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
    fn help_lists_every_command_with_usage() {
        for c in COMMANDS {
            assert!(
                help_text().contains(&format!("/{}", c.name)),
                "/{} missing from help",
                c.name
            );
            assert!(!c.summary.is_empty());
        }
        assert!(help_text().contains("Tip: just talk to me"));
        assert!(help_for("remind").unwrap().contains("/remind <when>"));
        assert!(help_for("nope").is_none());
    }

    #[test]
    fn telegram_menu_only_lists_flagged_commands() {
        assert!(COMMANDS.iter().filter(|c| c.telegram).count() >= 10);
        assert!(COMMANDS.iter().all(|c| c.summary.len() <= 256));
    }

    #[test]
    fn cadence_renders_anchored_and_intervals() {
        let anchored = ka_agent_core::CronSpec {
            text: "x".into(),
            source: "t".into(),
            recurrence_ms: Some(86_400_000),
            mode: ka_agent_core::CronMode::Agent,
            at_local: Some("08:00".into()),
            weekday: None,
        };
        let weekly = ka_agent_core::CronSpec {
            weekday: Some(0),
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

    #[test]
    fn weekday_names_map_to_monday_zero() {
        assert_eq!(weekday_from_name("mon"), Some(0));
        assert_eq!(weekday_from_name("Friday"), Some(4));
        assert_eq!(weekday_from_name("sunday"), Some(6));
        assert_eq!(weekday_from_name("noday"), None);
    }
}
