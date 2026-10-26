//! The new assistant tools (plan 2.4): reminders the model can set, files
//! it can send to the operator, calendar lookups, skill proposals — plus
//! the memory candidate's target file.

use serde_json::{Value, json};

use super::{ToolCtx, ToolFuture};

// ---------------------------------------------------------------- remind

pub(crate) fn remind_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "action": {"type": "string", "enum": ["add", "list", "cancel"], "description": "what to do"},
            "text": {"type": "string", "description": "the reminder text (add)"},
            "when": {"type": "string", "description": "in 2h30m | at 18:30 | tomorrow 9am | fri 17:00 | 2026-11-03 10:00 | RFC 3339 (add)"},
            "id": {"type": "string", "description": "reminder id or prefix (cancel)"}
        },
        "required": ["action"]
    })
}

pub(crate) fn remind<'a>(ctx: &'a ToolCtx, args: Value) -> ToolFuture<'a> {
    Box::pin(async move {
        match args["action"].as_str().ok_or("missing 'action'")? {
            "add" => {
                let text = args["text"].as_str().unwrap_or("").trim().to_string();
                if text.is_empty() || text.chars().count() > 500 {
                    return Err("text must be 1-500 chars".into());
                }
                let when = args["when"].as_str().ok_or("missing 'when'")?;
                // RFC 3339 first (the unambiguous form), then the same
                // grammar `/remind` accepts, in the operator's zone.
                let at_ms = match when.parse::<jiff::Timestamp>() {
                    Ok(ts) => ts.as_millisecond(),
                    Err(_) => {
                        let now = ctx.runtime.now_zoned();
                        let tokens: Vec<&str> = when.split_whitespace().collect();
                        let parsed = ka_agent_core::when::parse_when(&tokens, &now)
                            .ok_or_else(|| format!("cannot read {when:?} as a time"))?;
                        if parsed.consumed == 0 {
                            return Err(format!("cannot read {when:?} as a time"));
                        }
                        parsed.at_ms
                    }
                };
                if at_ms <= ka_agent_core::now_ms() {
                    return Err("that time is in the past".into());
                }
                let pending = ctx
                    .runtime
                    .scheduled_tasks()
                    .map_err(|e| format!("scheduler: {e}"))?
                    .iter()
                    .filter(|t| t.kind == "reminder")
                    .count();
                if pending >= 100 {
                    return Err(format!(
                        "{pending} reminders are pending (limit 100) — cancel some first"
                    ));
                }
                let id = ctx
                    .runtime
                    .add_reminder_at(at_ms, &text)
                    .map_err(|e| e.to_string())?;
                let now = ctx.runtime.now_zoned();
                let when_text = render_when(at_ms, &now);
                Ok(format!(
                    "reminder {} set for {when_text} ({})",
                    ka_agent_core::ids::short_id(id.as_str()),
                    now.time_zone().iana_name().unwrap_or("UTC")
                ))
            }
            "list" => {
                let rows = ctx
                    .runtime
                    .scheduled_tasks()
                    .map_err(|e| format!("scheduler: {e}"))?;
                let mut lines: Vec<String> = rows
                    .iter()
                    .filter(|t| t.kind == "reminder")
                    .filter_map(|t| {
                        let at = t.scheduled_at_ms?;
                        Some(format!(
                            "{} — {}",
                            render_when(at, &ctx.runtime.now_zoned()),
                            t.title
                        ))
                    })
                    .collect();
                if lines.is_empty() {
                    return Ok("no pending reminders".to_string());
                }
                lines.sort();
                Ok(lines.join("\n"))
            }
            "cancel" => {
                let id = args["id"].as_str().unwrap_or("");
                match ctx
                    .runtime
                    .cancel_scheduled(id)
                    .map_err(|e| e.to_string())?
                {
                    ka_agent_runtime::CancelOutcome::Cancelled(what) => {
                        Ok(format!("cancelled {what}"))
                    }
                    ka_agent_runtime::CancelOutcome::NotFound => {
                        Err(format!("no pending reminder matches {id:?}"))
                    }
                    ka_agent_runtime::CancelOutcome::Ambiguous(_) => {
                        Err(format!("ambiguous id {id:?} — be more specific"))
                    }
                }
            }
            other => Err(format!("unknown action {other:?}; use add|list|cancel")),
        }
    })
}

/// `Sat 25 Oct 18:30` in the operator's zone.
fn render_when(at_ms: i64, now: &jiff::Zoned) -> String {
    let z = jiff::Timestamp::from_millisecond(at_ms)
        .map(|t| t.to_zoned(now.time_zone().clone()))
        .unwrap_or_else(|_| now.clone());
    z.strftime("%a %e %b %H:%M").to_string()
}

// -------------------------------------------------------------- send_file

pub(crate) fn send_file_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "path": {"type": "string", "description": "file path (relative to the workspace, or in attachments)"},
            "caption": {"type": "string", "description": "a short caption for the operator"}
        },
        "required": ["path"]
    })
}

pub(crate) fn send_file<'a>(ctx: &'a ToolCtx, args: Value) -> ToolFuture<'a> {
    Box::pin(async move {
        let raw = args["path"].as_str().ok_or("missing 'path'")?;
        let caption = args
            .get("caption")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let p = std::path::Path::new(raw);
        let joined = if p.is_absolute() {
            p.to_path_buf()
        } else {
            ctx.runtime.workspace().join(p)
        };
        let real = std::fs::canonicalize(&joined).map_err(|e| format!("{raw}: {e}"))?;
        let ws = std::fs::canonicalize(ctx.runtime.workspace())
            .unwrap_or_else(|_| ctx.runtime.workspace());
        let attachments = std::fs::canonicalize(ctx.paths.root().join("attachments"))
            .unwrap_or_else(|_| ctx.paths.root().join("attachments"));
        if !real.starts_with(&ws) && !real.starts_with(&attachments) {
            return Err("send_file only sends files from the workspace or attachments".to_string());
        }
        let meta = std::fs::metadata(&real).map_err(|e| format!("{raw}: {e}"))?;
        if !meta.is_file() {
            return Err(format!("{raw} is not a file"));
        }
        const MAX: u64 = 50 * 1024 * 1024;
        if meta.len() > MAX {
            return Err(format!(
                "{raw} is {} MB (the cap is 50 MB)",
                meta.len() / (1024 * 1024)
            ));
        }
        let name = real
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "file".to_string());
        ctx.runtime.emit(ka_agent_runtime::AgentEvent::File {
            path: real.to_string_lossy().into_owned(),
            name: name.clone(),
            caption: caption.clone(),
        });
        Ok(format!("sent {name} ({} bytes)", meta.len()))
    })
}

// --------------------------------------------------------- calendar_events

pub(crate) fn calendar_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "from": {"type": "string", "description": "first day, YYYY-MM-DD (default today)"},
            "days": {"type": "integer", "description": "how many days, 1-14 (default 1)"}
        }
    })
}

pub(crate) fn calendar_events<'a>(ctx: &'a ToolCtx, args: Value) -> ToolFuture<'a> {
    Box::pin(async move {
        let Some(source) = crate::calendar::configured_source(&ctx.store) else {
            return Err("no calendar configured (/calendar add <url>)".to_string());
        };
        let tz = ctx.runtime.config().time_zone();
        let now = ctx.runtime.now_zoned();
        let from = match args["from"].as_str() {
            Some(d) => d
                .parse::<jiff::civil::Date>()
                .map_err(|e| format!("bad 'from' {d:?}: {e}"))?,
            None => now.date(),
        };
        let days = args["days"].as_u64().unwrap_or(1).clamp(1, 14) as i64;
        let to = from
            .checked_add(jiff::Span::new().days(days))
            .map_err(|e| format!("date range: {e}"))?;
        let store = std::sync::Arc::clone(&ctx.store);
        let events = tokio::task::spawn_blocking(move || {
            crate::calendar::events_between(&source, &store, &tz, from, to)
        })
        .await
        .map_err(|e| format!("calendar task: {e}"))??;
        if events.is_empty() {
            return Ok(format!("no events in the {days}-day window from {from}"));
        }
        let mut out = Vec::new();
        for e in events {
            let when = if e.all_day {
                e.start.strftime("%a %e %b (all day)").to_string()
            } else {
                e.start.strftime("%a %e %b %H:%M").to_string()
            };
            out.push(format!("{when} {}", e.summary));
        }
        Ok(out.join("\n"))
    })
}

// --------------------------------------------------------- skill_propose

pub(crate) fn skill_propose_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "name": {"type": "string", "description": "skill name, [a-z0-9-] (unique in the library)"},
            "description": {"type": "string", "description": "one line: when to use it (1-200 chars)"},
            "body": {"type": "string", "description": "the SKILL.md body (markdown, <= 8 KiB)"}
        },
        "required": ["name", "description", "body"]
    })
}

pub(crate) fn skill_propose<'a>(ctx: &'a ToolCtx, args: Value) -> ToolFuture<'a> {
    Box::pin(async move {
        let name = args["name"].as_str().unwrap_or("").trim();
        let description = args["description"].as_str().unwrap_or("").trim();
        let body = args["body"].as_str().unwrap_or("");
        if !ka_agent_runtime::skills::valid_skill_name(name) {
            return Err(format!(
                "bad skill name {name:?}: [a-z0-9-] only, 1-48 chars"
            ));
        }
        if description.is_empty() || description.chars().count() > 200 {
            return Err("description must be 1-200 chars".into());
        }
        if body.trim().is_empty() {
            return Err("body must not be empty".into());
        }
        if body.len() > 8192 {
            return Err("body is over 8 KiB".into());
        }
        // Reuse the memory proposal screening: rejects invisible/zero-width
        // characters and credential shapes (a skill body is instructions,
        // so the instruction-shape rule is skipped — same split as the
        // persona writer).
        ka_agent_memory::screen_invisible(body)?;
        ka_agent_memory::screen_credentials(body)?;
        let lib = ka_agent_core::layout::base_dir().join("skills");
        if lib.join(name).exists() {
            return Err(format!("skill {name:?} already exists in the library"));
        }
        let id = format!(
            "skill_{}",
            ka_agent_core::ids::short_id(&ka_agent_core::MemoryId::generate().to_string())
        );
        ctx.store
            .add_skill_candidate(&id, name, description, body)
            .map_err(|e| format!("store: {e}"))?;
        ctx.runtime
            .emit(ka_agent_runtime::AgentEvent::SkillProposal {
                id: id.clone(),
                name: name.to_string(),
                description: description.to_string(),
            });
        Ok(format!(
            "proposed skill {name} ({id}) — the operator reviews it in /skills review"
        ))
    })
}
