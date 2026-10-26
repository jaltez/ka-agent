//! `history_search` / `history_show`: the operator's past conversations.

use serde_json::{Value, json};

use super::{ToolCtx, ToolFuture};

pub(crate) fn search_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "query": {"type": "string", "description": "words to find"},
            "limit": {"type": "integer", "description": "max hits, 1-10 (default 5)"}
        },
        "required": ["query"]
    })
}

pub(crate) fn search<'a>(ctx: &'a ToolCtx, args: Value) -> ToolFuture<'a> {
    Box::pin(async move {
        let query = args["query"].as_str().unwrap_or("").trim();
        if query.is_empty() {
            return Err("missing 'query'".into());
        }
        let limit = args["limit"].as_u64().unwrap_or(5).clamp(1, 10) as usize;
        let hits = ctx
            .store
            .search_episodes(query, limit)
            .map_err(|e| format!("search: {e}"))?;
        if hits.is_empty() {
            return Ok(format!("nothing found for {query:?}"));
        }
        Ok(hits
            .iter()
            .map(|h| {
                let when = jiff::Timestamp::from_millisecond(h.at_ms)
                    .map(|t| t.strftime("%Y-%m-%d").to_string())
                    .unwrap_or_default();
                format!(
                    "[{}] {when} {}: {}",
                    ka_agent_core::ids::short_id(&h.turn),
                    h.role,
                    h.snippet
                )
            })
            .collect::<Vec<_>>()
            .join("\n"))
    })
}

pub(crate) fn show_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "turn": {"type": "string", "description": "turn id (or its tail) from history_search"}
        },
        "required": ["turn"]
    })
}

pub(crate) fn show<'a>(ctx: &'a ToolCtx, args: Value) -> ToolFuture<'a> {
    Box::pin(async move {
        let turn = args["turn"].as_str().unwrap_or("").trim();
        if turn.is_empty() {
            return Err("missing 'turn'".into());
        }
        let t = ctx
            .store
            .find_turn_by_prefix(turn)
            .map_err(|e| format!("store: {e}"))?
            .ok_or_else(|| format!("no turn {turn:?}"))?;
        let answer: String = ctx
            .store
            .chunks_for_turn(&t.id)
            .map_err(|e| format!("store: {e}"))?
            .into_iter()
            .map(|c| c.text)
            .collect();
        // Bounded: one turn must not flood the context.
        const MAX: usize = 4_000;
        Ok(format!(
            "operator: {}\n\nanswer: {}",
            ka_agent_core::truncate_at_char(&t.inbound_text, MAX / 4),
            ka_agent_core::truncate_at_char(&answer, MAX)
        ))
    })
}
