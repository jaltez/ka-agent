//! `memory_propose`: a durable-memory proposal for operator review.

use serde_json::{Value, json};

use super::{Effect, ToolCtx, ToolFuture, turn_principal};

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
        if ctx
            .runtime
            .session_private_for(&crate::tools::turn_principal(ctx))
        {
            return Err("this is a private conversation — memory proposals are off here".into());
        }
        // Automation turns never write personal memory: a cron reading
        // what an earlier cron proposed is a feedback loop (B15b).
        if let Some(src) = super::turnctx(ctx, "source")
            && ka_agent_core::is_scheduled_source(&src)
        {
            return Err("this turn is scheduled — automation turns cannot propose memory".into());
        }
        // Quota/warm-up gates (B15c): do not spend the extraction slot
        // when the budget window is nearly spent, a rate limit is
        // cooling down, or the session is still warming up.
        let who = turn_principal(ctx);
        match ctx.runtime.budget_verdict_for(&who.username) {
            ka_agent_core::BudgetVerdict::Warn { .. } | ka_agent_core::BudgetVerdict::Exceeded => {
                return Err("budget window is nearly spent — propose this memory later".into());
            }
            ka_agent_core::BudgetVerdict::Ok => {}
        }
        let cooled = ctx
            .store
            .last_rate_limit_at(&who.username)
            .map_err(|e| format!("rate-limit lookup: {e}"))?
            .is_some_and(|at| ka_agent_core::now_ms() - at < 30 * 60 * 1000);
        if cooled {
            return Err("a rate limit is cooling down — propose this memory later".into());
        }
        let min_turns = ctx.runtime.config().memory.extraction_min_turns;
        if min_turns > 0 {
            let count = ctx
                .runtime
                .session_turn_count_for(&who.username)
                .unwrap_or(i64::MAX);
            if count < min_turns as i64 {
                return Err(format!(
                    "session is still warming up ({count} of {min_turns} turns) — propose this memory later"
                ));
            }
        }
        let text = args["text"].as_str().ok_or("missing 'text'")?;
        if !proposal_len_ok(text) {
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
        // The proposal belongs to the role whose turn made it: accepted
        // memory lands in that role's root, never the admin's by accident.
        ctx.store
            .add_candidate_for_user(
                &id,
                "model",
                text,
                None,
                file,
                who.role.as_str(),
                &who.username,
            )
            .map_err(|e| format!("candidate: {e}"))?;
        Ok(format!(
            "proposed for review: {id} (the operator will see it in /memory review)"
        ))
    })
}

/// The contract says 300 characters, not 300 bytes: a Spanish accented
/// proposal near the limit must not be over-refused.
fn proposal_len_ok(text: &str) -> bool {
    let chars = text.chars().count();
    chars > 0 && chars <= 300
}

#[cfg(test)]
mod tests {
    #[test]
    fn proposal_length_counts_characters_not_bytes() {
        assert!(super::proposal_len_ok(&"ñ".repeat(151)));
        assert!(super::proposal_len_ok(&"a".repeat(300)));
        assert!(!super::proposal_len_ok(&"ñ".repeat(301)));
        assert!(!super::proposal_len_ok(""));
    }
}
