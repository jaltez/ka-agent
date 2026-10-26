//! `memory_propose`: a durable-memory proposal for operator review.

use serde_json::{Value, json};

use super::{Effect, ToolCtx, ToolFuture};

pub(crate) fn propose_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "text": {"type": "string", "description": "The memory fact, third person, atomic"},
            "file": {"type": "string", "enum": ["facts", "preferences", "environment", "profile"],
                     "description": "which memory file it belongs in (default facts)"}
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
        let file = args["file"].as_str().unwrap_or("facts");
        if !matches!(file, "facts" | "preferences" | "environment" | "profile") {
            return Err(format!(
                "unknown file {file:?}: facts | preferences | environment | profile"
            ));
        }
        let file = (file != "facts").then_some(file);
        let id = ka_agent_core::MemoryId::generate();
        ctx.store
            .add_candidate_to(&id, "model", text, None, file)
            .map_err(|e| format!("candidate: {e}"))?;
        Ok(format!(
            "proposed for review: {id} (the operator will see it in /memory review)"
        ))
    })
}
