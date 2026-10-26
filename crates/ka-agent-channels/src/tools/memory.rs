//! `memory_propose`: a durable-memory proposal for operator review.

use serde_json::{Value, json};

use super::{Effect, ToolCtx, ToolFuture};

pub(crate) fn propose_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "text": {"type": "string", "description": "The memory fact, third person, atomic"}
        },
        "required": ["text"]
    })
}

pub(crate) fn propose<'a>(ctx: &'a ToolCtx, args: Value) -> ToolFuture<'a> {
    Box::pin(async move {
        let _ = Effect::Propose;
        let text = args["text"].as_str().ok_or("missing 'text'")?;
        if text.is_empty() || text.len() > 300 {
            return Err("text must be 1-300 chars".into());
        }
        ka_agent_memory::screen_proposal(text)?;
        let id = ka_agent_core::MemoryId::generate();
        ctx.store
            .add_candidate(&id, "model", text, None)
            .map_err(|e| format!("candidate: {e}"))?;
        Ok(format!(
            "proposed for review: {id} (the operator will see it in /memory review)"
        ))
    })
}
