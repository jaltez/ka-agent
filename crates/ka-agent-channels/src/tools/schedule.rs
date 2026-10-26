//! `cron_list` / `cron_manage`: scheduled agent jobs. Split from the old
//! combined `cron_manage` (2.3): listing is a read, changes are `Schedule`.

use serde_json::{Value, json};

use super::{ToolCtx, ToolFuture};

/// Shortest recurrence the model may schedule: every fire is a full agent
/// turn, so a runaway `every 1s` would burn the budget in minutes.
const MIN_MODEL_CRON_EVERY_MS: u64 = 60_000;
/// Live crons the model may hold at once (operator crons don't count).
const MAX_MODEL_CRONS: usize = 50;

pub(crate) fn list_schema() -> Value {
    json!({ "type": "object", "properties": {} })
}

pub(crate) fn list<'a>(ctx: &'a ToolCtx, _args: Value) -> ToolFuture<'a> {
    Box::pin(async move {
        let crons = ctx.store.cron_tasks().map_err(|e| format!("store: {e}"))?;
        if crons.is_empty() {
            return Ok("no crons".to_string());
        }
        Ok(crate::commands::cron_list_text(
            &crons,
            &ctx.runtime.now_zoned(),
        ))
    })
}

pub(crate) fn manage_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "action": {
                "type": "string",
                "enum": ["schedule", "pause", "resume", "remove", "trigger"],
                "description": "what to do"
            },
            "text": {"type": "string", "description": "the prompt a scheduled run sends (schedule)"},
            "in": {"type": "string", "description": "one-shot delay, e.g. '10m' (schedule)"},
            "every": {"type": "string", "description": "interval, e.g. '1h' (schedule; min 60s)"},
            "repeat": {"type": "string", "description": "local-time recurrence, e.g. 'every day at 08:00' (schedule)"},
            "at": {"type": "string", "description": "absolute RFC 3339 with the operator offset (schedule)"},
            "id": {"type": "string", "description": "cron id or prefix (pause/resume/remove/trigger)"}
        },
        "required": ["action"]
    })
}

pub(crate) fn manage<'a>(ctx: &'a ToolCtx, args: Value) -> ToolFuture<'a> {
    Box::pin(async move {
        let action = args["action"].as_str().ok_or("missing 'action'")?;
        match action {
            "schedule" => {
                // Loop guard: a scheduled run may not schedule more jobs
                // (a cron that creates crons runs away). The turn source
                // is the live one — no store meta row in between.
                if ctx
                    .runtime
                    .current_turn_source()
                    .is_some_and(|s| s == "cron" || s.starts_with("cron:"))
                {
                    return Err(
                        "refused: this turn was started by a scheduled job, and scheduled jobs cannot schedule more (ask the operator to add it)".into(),
                    );
                }
                let text = args["text"].as_str().ok_or("missing 'text'")?;
                if text.trim().is_empty() {
                    return Err("text must not be empty".into());
                }
                let every = args["every"].as_str();
                let once = args["in"].as_str();
                let at = args["at"].as_str();
                // `repeat`: anchored local-time recurrence, in the
                // operator's zone.
                let tz = ctx.runtime.config().time_zone();
                let mut anchored: Option<(ka_agent_core::when::DayRule, (u8, u8))> = None;
                if let Some(repeat) = args["repeat"].as_str() {
                    if every.is_some() || once.is_some() || at.is_some() {
                        return Err("give 'repeat' alone (not with 'in', 'every' or 'at')".into());
                    }
                    let words: Vec<&str> = repeat.split_whitespace().collect();
                    let words = words.strip_prefix(&["every"]).unwrap_or(&words);
                    let (rule, clock, used) = ka_agent_core::when::parse_recurrence(words)
                        .ok_or_else(|| format!("bad 'repeat' {repeat:?}: use e.g. 'every weekday at 9', 'every mon,wed at 18:00', 'every month on the 1st at 9'"))?;
                    if used != words.len() {
                        return Err(format!(
                            "bad 'repeat' {repeat:?}: unexpected words after the time"
                        ));
                    }
                    anchored = Some((rule, clock));
                }
                let (recurrence_ms, first_at_ms) = match (every, once, at) {
                    _ if anchored.is_some() => {
                        let (rule, (hh, mm)) = anchored.clone().ok_or("internal: repeat")?;
                        let now = ctx.runtime.now_zoned();
                        (
                            Some(86_400_000),
                            ka_agent_core::when::next_local(hh, mm, &rule, &now),
                        )
                    }
                    (Some(d), None, None) => {
                        let ms = ka_agent_core::parse_duration(d)
                            .ok_or_else(|| format!("bad duration {d:?}; use Ns, Nm, Nh or Nd"))?;
                        if ms < MIN_MODEL_CRON_EVERY_MS {
                            return Err(format!(
                                "'every' must be at least {}s — each run is a full agent turn",
                                MIN_MODEL_CRON_EVERY_MS / 1000
                            ));
                        }
                        (Some(ms as i64), ka_agent_core::now_ms() + ms as i64)
                    }
                    (None, Some(d), None) => {
                        let ms = ka_agent_core::parse_duration(d)
                            .ok_or_else(|| format!("bad duration {d:?}; use Ns, Nm, Nh or Nd"))?;
                        (None, ka_agent_core::now_ms() + ms as i64)
                    }
                    (None, None, Some(rfc3339)) => {
                        let ts: jiff::Timestamp = rfc3339.parse().map_err(|e| format!("bad 'at' {rfc3339:?}: {e} (use RFC 3339 with offset, e.g. 2026-10-20T09:00:00+02:00)"))?;
                        if ts.as_millisecond() <= ka_agent_core::now_ms() {
                            return Err("'at' is in the past".into());
                        }
                        (None, ts.as_millisecond())
                    }
                    _ => {
                        return Err(
                            "give exactly one of 'in' (one-shot), 'every' (interval), 'repeat' (local-time days) or 'at' (absolute RFC 3339)".into(),
                        );
                    }
                };
                let live = ctx
                    .store
                    .cron_tasks()
                    .map_err(|e| format!("store: {e}"))?
                    .iter()
                    .filter(|t| {
                        t.payload
                            .as_deref()
                            .and_then(ka_agent_core::CronSpec::from_payload)
                            .is_some_and(|c| c.source == "kagent")
                    })
                    .count();
                if live >= MAX_MODEL_CRONS {
                    return Err(format!(
                        "{live} model-scheduled crons are live (limit {MAX_MODEL_CRONS}) — remove some first"
                    ));
                }
                let mut spec = ka_agent_core::CronSpec {
                    text: text.to_string(),
                    source: "kagent".into(),
                    recurrence_ms,
                    mode: ka_agent_core::CronMode::Agent,
                    at_local: anchored
                        .as_ref()
                        .map(|(_, (hh, mm))| format!("{hh:02}:{mm:02}")),
                    weekdays: Vec::new(),
                    month_day: None,
                    gate: None,
                };
                if let Some((rule, _)) = &anchored {
                    spec.set_day_rule(rule);
                }
                let _ = tz;
                let mut task = ka_agent_core::Task::new(spec.text.clone(), "cron", false);
                task.scheduled_at_ms = Some(first_at_ms);
                task.payload = Some(spec.to_payload());
                ctx.store
                    .insert_task(&task)
                    .map_err(|e| format!("insert: {e}"))?;
                Ok(format!(
                    "cron {} scheduled ({})",
                    ka_agent_core::ids::short_id(task.id.as_str()),
                    match (&anchored, recurrence_ms) {
                        (Some((rule, (hh, mm))), _) => {
                            format!("{} at {hh:02}:{mm:02}", rule.describe())
                        }
                        (None, Some(ms)) => {
                            format!("every {}", crate::commands::fmt_interval(ms))
                        }
                        (None, None) => "once".to_string(),
                    }
                ))
            }
            "pause" | "resume" | "remove" | "trigger" => {
                use ka_agent_runtime::cron_ops;
                let task = cron_ops::resolve(&ctx.store, args["id"].as_str().unwrap_or(""))?;
                let short = ka_agent_core::ids::short_id(task.id.as_str());
                match action {
                    "pause" => {
                        cron_ops::pause(&ctx.store, &task)?;
                        Ok(format!("cron {short} paused"))
                    }
                    "remove" => {
                        cron_ops::remove(&ctx.store, &task)?;
                        Ok(format!("cron {short} removed"))
                    }
                    "trigger" => {
                        cron_ops::trigger(&ctx.store, &task)?;
                        Ok(format!(
                            "cron {short} armed; fires on the next scheduler tick"
                        ))
                    }
                    _ => {
                        cron_ops::resume(&ctx.store, &task, &ctx.runtime.now_zoned())?;
                        Ok(format!("cron {short} resumed"))
                    }
                }
            }
            other => Err(format!(
                "unknown action {other:?}; use schedule|list|pause|resume|remove|trigger"
            )),
        }
    })
}
