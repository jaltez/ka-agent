//! `scout`: one read-only research child, driven by the runtime.

use serde_json::{Value, json};

use super::{Effect, ToolCtx, ToolFuture, turn_principal};

pub(crate) fn question_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "question": {"type": "string",
                         "description": "the one question the child must answer; it sees nothing else of this conversation"}
        },
        "required": ["question"]
    })
}

pub(crate) fn run<'a>(ctx: &'a ToolCtx, args: Value) -> ToolFuture<'a> {
    Box::pin(async move {
        let _ = Effect::Spawn;
        let question = args["question"].as_str().map(str::trim).unwrap_or("");
        if question.is_empty() {
            return Err("missing 'question'".into());
        }
        if question.chars().count() > 2000 {
            return Err("question too long (max 2000 chars) — ask a narrower question".into());
        }
        let who = turn_principal(ctx);
        ctx.runtime
            .run_scout(&who, question)
            .await
            .map_err(|e| format!("scout: {e}"))
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::super::{BUILTINS, Effect, auto_allowed};

    #[test]
    fn spawn_asks_and_the_row_is_registered() {
        // A spawn spends a model turn and a worker slot: ask mode must
        // raise the card, so it never joins the auto-allow profile.
        assert!(!auto_allowed(Effect::Spawn));
        let row = BUILTINS
            .iter()
            .find(|tool| tool.name == "scout")
            .expect("scout row");
        assert_eq!(row.effect, Effect::Spawn);
        let schema = (row.schema)();
        assert_eq!(schema["required"][0], "question");
    }
}
