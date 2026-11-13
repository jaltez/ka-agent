//! `kb_search`: retrieval over the knowledge bases the acting user may read
//! (ADR-0004, D6/M1).
//!
//! Access is decided before the query runs: the user's effective KB set
//! (their groups' KBs, the ones they own, everything for the operator). An
//! unknown or unauthorized KB answers `no knowledge base matches` — never
//! "forbidden", so the tool leaks no existence information.
//!
//! Every hit is recorded in `turn_sources` with the KB's access groups, so a
//! derived result can be re-checked before it is shared (M3 provenance).

use serde_json::{Value, json};

use super::{ToolCtx, ToolFuture, turn_principal};

/// Hits per call (the plan's top-8 snippets).
const LIMIT: usize = 8;
/// Context chars around a match, per hit.
const SNIPPET_CHARS: usize = 600;

pub(crate) fn search_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "query": {"type": "string", "description": "Words to look for; every word must appear (any order)"},
            "kb": {"type": "string", "description": "Optional knowledge base name; omit to search every KB you can read"}
        },
        "required": ["query"]
    })
}

pub(crate) fn search<'a>(ctx: &'a ToolCtx, args: Value) -> ToolFuture<'a> {
    Box::pin(async move {
        let query = args["query"].as_str().unwrap_or("").trim();
        if query.is_empty() {
            return Err("kb_search needs a query".into());
        }
        let who = turn_principal(ctx);
        let mut effective = ctx
            .store
            .kb_effective(&who.username, &who.role)
            .map_err(|e| format!("kb access: {e}"))?;
        // A shared agent's knowledge joins the turn, still intersected with
        // what the user may read. Per-user key: global rows cross users once
        // turns run in parallel.
        if let Some(agent) = super::turnctx(ctx, "agent")
            .and_then(|name| ctx.store.get_agent(&name).ok().flatten())
        {
            for kb in agent.kbs {
                if effective.contains(&kb) {
                    continue;
                }
                if ctx
                    .store
                    .kb_can_read(&kb, &who.username, &who.role)
                    .unwrap_or(false)
                {
                    effective.push(kb);
                }
            }
        }
        let kb = args["kb"].as_str().unwrap_or("").trim();
        let scope: Vec<String> = if kb.is_empty() {
            effective.clone()
        } else if effective.iter().any(|k| k == kb) {
            vec![kb.to_string()]
        } else {
            // Unknown or unauthorized: the same answer for both.
            return Ok("no knowledge base matches".to_string());
        };
        if scope.is_empty() {
            return Ok("no knowledge base matches".to_string());
        }
        let hits = ctx
            .store
            .kb_search(&scope, query, LIMIT)
            .map_err(|e| format!("kb search: {e}"))?;
        if hits.is_empty() {
            return Ok(format!(
                "no matches for {query:?} in {}",
                scope.join(", ")
            ));
        }
        // Provenance: one row per cited document, with the KB's groups.
        // The actor's own current turn (per-user key: global rows cross
        // users once turns run in parallel).
        let turn = super::turnctx(ctx, "id").unwrap_or_default();
        let mut out = format!("{} hit(s) for {query:?}:\n", hits.len());
        for h in &hits {
            let groups = ctx.store.kb_access(&h.kb_name).unwrap_or_default();
            if !turn.is_empty() {
                let _ = ctx.store.record_turn_source(
                    &turn,
                    "kb",
                    &h.citation(),
                    &groups,
                );
            }
            let snippet = ka_agent_core::truncate_at_char(&h.snippet, SNIPPET_CHARS);
            out.push_str(&format!(
                "\n[{}] {}\n  {}\n",
                h.citation(),
                h.title,
                snippet.replace('\n', "\n  ")
            ));
        }
        Ok(out)
    })
}
