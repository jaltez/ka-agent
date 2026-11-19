//! `plan_update`: the model's write into the plan draft it was asked to
//! produce or execute (v4 F12). Only `drafting`/`draft` rows are
//! writable — an approved plan is operator property, and a live
//! (`executing`) one would race its own verify cycle.

use serde_json::{Value, json};

use super::{ToolCtx, ToolFuture, turn_principal, turnctx};

pub(crate) fn update_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "id": {"type": "string",
                   "description": "The plan id (defaults to the plan this turn belongs to, e.g. a /plan draft or /plan run turn)"},
            "body": {"type": "string",
                     "description": "The full new plan body as markdown (numbered steps; a final `Verify: <shell command>` line names its check)"}
        },
        "required": ["body"]
    })
}

pub(crate) fn update<'a>(ctx: &'a ToolCtx, args: Value) -> ToolFuture<'a> {
    Box::pin(async move {
        let body = args["body"].as_str().ok_or("missing 'body'")?;
        if body.trim().is_empty() {
            return Err("body must not be empty".into());
        }
        let id = match args["id"].as_str().filter(|s| !s.trim().is_empty()) {
            Some(id) => id.trim().to_string(),
            // Default: the plan this turn belongs to (`plan:<id>` source).
            None => turnctx(ctx, "source")
                .as_deref()
                .and_then(|s| s.strip_prefix("plan:"))
                .map(str::to_string)
                .ok_or("no plan id given and this turn is not a plan turn")?,
        };
        let store = &ctx.store;
        let plan = store
            .get_plan(&id)
            .map_err(|e| format!("plan lookup: {e}"))?
            .ok_or_else(|| format!("no plan {id:?}"))?;
        // A plan belongs to its owner: the model never rewrites another
        // user's artifact.
        if plan.username != turn_principal(ctx).username {
            return Err(format!("plan {id} belongs to another user"));
        }
        if plan.status == ka_agent_store::PlanStatus::Approved {
            return Err(
                "plan is approved — its body is frozen; ask the operator to /plan set it".into(),
            );
        }
        store
            .set_plan_body(&id, body)
            .map_err(|e| format!("plan update: {e}"))?;
        Ok(format!("plan {id} body updated ({})", plan.status))
    })
}
