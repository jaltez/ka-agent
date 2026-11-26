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
        // what an earlier cron proposed is a feedback loop (B15b). The
        // source gate covers crons and schedule triggers; the memory scope
        // covers a webhook fire under its default `Automation` scope.
        if let Some(src) = super::turnctx(ctx, "source")
            && ka_agent_core::is_scheduled_source(&src)
        {
            return Err("this turn is scheduled — automation turns cannot propose memory".into());
        }
        if runs_with_automation_memory(ctx) {
            return Err(
                "this turn runs with automation memory — automation turns cannot propose memory"
                    .into(),
            );
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

/// Does this turn run under the `Automation` memory scope, read from its
/// `turnctx:<user>:memory_scope` context? A webhook fire under the default
/// scope is an automation turn for memory just like a cron (B15b), so both
/// memory tools refuse it. A context without the field predates it and is
/// not automation: the scheduled-source gate still covers those turns.
pub(super) fn runs_with_automation_memory(ctx: &ToolCtx) -> bool {
    super::turnctx(ctx, "memory_scope")
        .as_deref()
        .and_then(ka_agent_core::TriggerMemoryScope::parse)
        == Some(ka_agent_core::TriggerMemoryScope::Automation)
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

#[cfg(test)]
mod automation_scope_tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use std::sync::Arc;

    use ka_agent_runtime::surface::Surface;
    use ka_agent_runtime::{Envelope, Paths};
    use ka_agent_store::Store;

    struct Noop;

    impl Surface for Noop {
        fn emit(&self, _ev: &Envelope) {}
    }

    /// Call one memory tool in a turn that carries the given memory scope.
    /// The source is a webhook's, so only the scope gate can refuse.
    async fn call_with_scope(tool: &str, scope: &str) -> (String, bool) {
        // The tests run in parallel: a per-call sequence, not the clock,
        // keeps two runtimes off one data root.
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!("ka-mem-scope-{}-{n}", std::process::id()));
        let store = Arc::new(Store::open(&root.join("state.db")).unwrap());
        let who = ka_agent_core::Principal::operator();
        let cfg = ka_agent_core::DaemonConfig {
            workspace: root.join("ws"),
            ..ka_agent_core::DaemonConfig::default()
        };
        let runtime = ka_agent_runtime::Runtime::new(
            cfg,
            Paths::new(root.clone()),
            Arc::clone(&store),
            ka_agent_port::WorkerPort::default(),
            Arc::new(Noop),
        )
        .unwrap();
        store
            .meta_set(
                &ka_agent_core::layout::turnctx_key(&who.username, "source"),
                "trigger:task_1",
            )
            .unwrap();
        store
            .meta_set(
                &ka_agent_core::layout::turnctx_key(&who.username, "memory_scope"),
                scope,
            )
            .unwrap();
        let ctx = super::ToolCtx {
            runtime,
            store: Arc::clone(&store),
            paths: Paths::new(root.clone()),
            actor: who,
        };
        let out = super::super::call(
            &ctx,
            tool,
            serde_json::json!({"query": "tea", "text": "likes tea"}),
        )
        .await;
        let _ = std::fs::remove_dir_all(&root);
        out
    }

    #[tokio::test]
    async fn memory_tools_refuse_a_turn_under_the_automation_scope() {
        for tool in ["memory_search", "memory_propose"] {
            let (text, is_error) = call_with_scope(tool, "automation").await;
            assert!(is_error, "{tool} should refuse: {text}");
            assert!(text.contains("automation memory"), "{tool}: {text}");
        }
    }

    #[tokio::test]
    async fn the_memory_scope_gate_lets_every_other_turn_through() {
        // The creator's opt-in, and a context that predates the field.
        for scope in ["creator", ""] {
            let (text, is_error) = call_with_scope("memory_search", scope).await;
            assert!(!is_error, "scope {scope:?}: {text}");
            assert!(text.contains("nothing found"), "scope {scope:?}: {text}");
        }
    }
}
