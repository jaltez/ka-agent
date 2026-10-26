//! The daemon-hosted tools (plan 2.3): the `kagent__*` family the engine
//! sees through the thin `__mcp` relay. Every tool is one [`ToolSpec`] with
//! a declared [`Effect`] (what the approval profile may auto-allow) and an
//! async handler that runs in the daemon — ordinary requests over IPC, no
//! second store reader, no env-based data root.
//!
//! Tool names are bare (`memory_propose`); the engine exposes them as
//! `kagent__memory_propose`. Plugin tools are namespaced `<id>.<tool>`.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use ka_agent_runtime::Runtime;
use ka_agent_runtime::paths::Paths;
use ka_agent_store::Store;
use serde_json::Value;

pub mod github;
pub mod history;
pub mod http;
pub mod memory;
pub mod notes;
pub mod plugins;
pub mod schedule;

/// What a tool can do to the world. The approval profile auto-allows
/// `Read | Propose | LocalWrite`; `Schedule`/`ExternalWrite` still ask in
/// guarded mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Effect {
    /// Reads state; never changes anything.
    Read,
    /// Stages a proposal for the operator (memory/skill candidates).
    Propose,
    /// Writes inside ka-agent's own data root (notes, reminders, files).
    LocalWrite,
    /// Creates or changes scheduled work.
    Schedule,
    /// Writes through an external service (GitHub, HTTP effects).
    ExternalWrite,
}

/// Everything a handler needs, built by the daemon per call.
pub struct ToolCtx {
    pub runtime: Runtime,
    pub store: Arc<Store>,
    pub paths: Paths,
}

pub type ToolFuture<'a> = Pin<Box<dyn Future<Output = Result<String, String>> + Send + 'a>>;

/// One registered tool.
pub struct ToolSpec {
    pub name: &'static str,
    pub description: &'static str,
    pub schema: fn() -> Value,
    pub effect: Effect,
    pub run: for<'a> fn(&'a ToolCtx, Value) -> ToolFuture<'a>,
}

/// Every built-in tool, in listing order.
pub static BUILTINS: &[ToolSpec] = &[
    ToolSpec {
        name: "memory_propose",
        description: "Propose a durable memory about the operator for their review. Keep it atomic (one fact, <=300 chars), state it in third person, and never include secrets. The operator reviews and accepts/rejects proposals; nothing is stored until they do.",
        schema: memory::propose_schema,
        effect: Effect::Propose,
        run: memory::propose,
    },
    ToolSpec {
        name: "history_search",
        description: "Search past conversations with the operator (all sessions) by words; returns up to 10 snippets with their turn ids and dates. Use it to recall what was said or decided before.",
        schema: history::search_schema,
        effect: Effect::Read,
        run: history::search,
    },
    ToolSpec {
        name: "history_show",
        description: "Show one past turn in full (the operator's message and the answer), by the turn id from history_search.",
        schema: history::show_schema,
        effect: Effect::Read,
        run: history::show,
    },
    ToolSpec {
        name: "notes_search",
        description: "Search the operator's local markdown notes under the notes/ directory. Returns matching lines with file paths.",
        schema: notes::search_schema,
        effect: Effect::Read,
        run: notes::search,
    },
    ToolSpec {
        name: "notes_append",
        description: "Append a line to a note file in the notes/ directory. Creates the file if it does not exist. File names: [a-z0-9-] only.",
        schema: notes::append_schema,
        effect: Effect::LocalWrite,
        run: notes::append,
    },
    ToolSpec {
        name: "notes_read",
        description: "Read a note file from the notes/ directory.",
        schema: notes::read_schema,
        effect: Effect::Read,
        run: notes::read,
    },
    ToolSpec {
        name: "http_get",
        description: "Fetch one HTTP(S) URL and return its text body (needs an operator grant for the host; private hosts need an extra grant).",
        schema: http::get_schema,
        effect: Effect::Read,
        run: http::get,
    },
    ToolSpec {
        name: "github_issues_list",
        description: "List issues of a GitHub repository.",
        schema: github::issues_list_schema,
        effect: Effect::Read,
        run: github::issues_list,
    },
    ToolSpec {
        name: "github_pulls_get",
        description: "Show one pull request of a GitHub repository.",
        schema: github::pulls_get_schema,
        effect: Effect::Read,
        run: github::pulls_get,
    },
    ToolSpec {
        name: "github_issue_comment",
        description: "Post a comment on a GitHub issue (external write, logged as an intent).",
        schema: github::issue_comment_schema,
        effect: Effect::ExternalWrite,
        run: github::issue_comment,
    },
    ToolSpec {
        name: "github_pull_review",
        description: "Post a review on a GitHub pull request (external write, logged as an intent).",
        schema: github::pull_review_schema,
        effect: Effect::ExternalWrite,
        run: github::pull_review,
    },
    ToolSpec {
        name: "cron_list",
        description: "List the scheduled agent jobs (crons) with their next fire time.",
        schema: schedule::list_schema,
        effect: Effect::Read,
        run: schedule::list,
    },
    ToolSpec {
        name: "cron_manage",
        description: "Schedule, pause, resume, remove or trigger a recurring agent job. Each run is a full agent turn: minimum interval 60 s.",
        schema: schedule::manage_schema,
        effect: Effect::Schedule,
        run: schedule::manage,
    },
];

/// May a call with this effect skip the approval card? Reads, proposals and
/// writes inside ka-agent's own data root never ask; scheduling and
/// external writes do (unless a rule says otherwise).
pub fn auto_allowed(effect: Effect) -> bool {
    matches!(effect, Effect::Read | Effect::Propose | Effect::LocalWrite)
}

/// Every tool the engine may see: built-ins plus enabled plugins.
pub async fn list(ctx: &ToolCtx) -> Vec<Value> {
    let mut out: Vec<Value> = BUILTINS
        .iter()
        .map(|t| {
            serde_json::json!({
                "name": t.name,
                "description": t.description,
                "inputSchema": (t.schema)(),
            })
        })
        .collect();
    out.extend(plugins::list(ctx).await);
    out
}

/// Run one tool call. `(text, is_error)`.
pub async fn call(ctx: &ToolCtx, name: &str, args: Value) -> (String, bool) {
    if let Some(spec) = BUILTINS.iter().find(|t| t.name == name) {
        return match (spec.run)(ctx, args).await {
            Ok(text) => (text, false),
            Err(e) => (e, true),
        };
    }
    match plugins::call(ctx, name, args).await {
        Ok(text) => (text, false),
        Err(e) => (e, true),
    }
}

/// The effect of a tool by name (plugins: manifest read → Read, else
/// ExternalWrite). Used by the approval profile to decide auto-allow.
pub async fn effect_of(ctx: &ToolCtx, name: &str) -> Effect {
    if let Some(spec) = BUILTINS.iter().find(|t| t.name == name) {
        return spec.effect;
    }
    plugins::effect_of(ctx, name).await
}
