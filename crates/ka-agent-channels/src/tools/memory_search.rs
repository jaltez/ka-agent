//! `memory_search`: accepted memory and past turns in one read-only call.
//!
//! The engine sees this through `kagent__memory_search`. It never writes,
//! and it is off in private conversations and scheduled turns: those
//! sessions must not read personal memory (B15b, privacy contract).

use serde_json::{Value, json};

use super::{ToolCtx, ToolFuture, turn_principal, turnctx};

pub(crate) fn schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "query": {"type": "string", "description": "words to find"},
            "kind": {
                "type": "string",
                "enum": ["all", "memory", "episode", "kb"],
                "description": "memory, past turns, both (default), or a pointer to kb_search"
            },
            "limit": {"type": "integer", "description": "max hits, 1-20 (default 8)"}
        },
        "required": ["query"]
    })
}

pub(crate) fn run<'a>(ctx: &'a ToolCtx, args: Value) -> ToolFuture<'a> {
    Box::pin(async move {
        let who = turn_principal(ctx);
        if ctx.runtime.session_private_for(&who) {
            return Err("this is a private conversation — memory search is off here".into());
        }
        if let Some(src) = turnctx(ctx, "source")
            && (src == "cron" || src.starts_with("cron:") || src == "briefing")
        {
            return Err("this turn is scheduled — memory search is off here".into());
        }
        let query = args["query"].as_str().unwrap_or("").trim();
        if query.is_empty() {
            return Err("missing 'query'".into());
        }
        let kind = args["kind"].as_str().unwrap_or("all");
        if kind == "kb" {
            return Ok(
                "memory_search covers accepted memory and past turns; use `kb_search` for knowledge-base documents"
                    .into(),
            );
        }
        let limit = args["limit"].as_u64().unwrap_or(8).clamp(1, 20) as usize;
        // Fetch a wider window for a kind-specific call: the merged list is
        // memory first, then episodes, so a narrow window would starve the
        // episode side.
        let window = if kind == "all" { limit } else { limit.max(20) };
        let hits = ctx
            .runtime
            .search_memory_and_episodes(query, window, &who)
            .map_err(|e| format!("memory search: {e}"))?;
        let hits: Vec<_> = hits
            .into_iter()
            .filter(|h| match kind {
                "memory" => h.kind == "memory",
                "episode" => h.kind == "episode",
                _ => true,
            })
            .take(limit)
            .collect();
        if hits.is_empty() {
            return Ok(format!("nothing found for {query:?}"));
        }
        let now = ctx.runtime.now_zoned();
        Ok(hits
            .into_iter()
            .map(|h| {
                let date = jiff::Timestamp::from_millisecond(h.at_ms)
                    .map(|ts| ts.to_zoned(now.time_zone().clone()))
                    .map(|d| format!("{:04}-{:02}-{:02}", d.year(), d.month() as u8, d.day()))
                    .unwrap_or_default();
                let tag = if h.kind == "memory" {
                    format!("mem:{}", ka_agent_core::ids::short_id(&h.id))
                } else {
                    format!("turn:{}", ka_agent_core::ids::short_id(&h.id))
                };
                format!("[{tag}] {date} {}: {}", h.detail, h.snippet)
            })
            .collect::<Vec<_>>()
            .join("\n"))
    })
}
