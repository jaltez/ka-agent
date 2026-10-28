//! Session-level commands: status, new session, stop, resume, model, mode,
//! quit.

use super::{CmdFuture, Ctx, Outcome};
use ka_agent_core::ids::short_id;
use ka_agent_runtime::Runtime;

pub(super) fn run_status<'a>(ctx: Ctx<'a>) -> CmdFuture<'a> {
    Box::pin(async move {
        match ctx.runtime.status_report() {
            Ok(report) => Outcome::Reply(report.render(&ctx.runtime.now_zoned())),
            Err(e) => Outcome::Reply(format!("! [status] {e}")),
        }
    })
}

pub(super) fn run_new<'a>(ctx: Ctx<'a>) -> CmdFuture<'a> {
    Box::pin(async move {
        match ctx.runtime.new_session().await {
            Ok(id) => Outcome::Reply(format!("new session {}", short_id(id.as_str()))),
            Err(e) => Outcome::Reply(format!("! [session] {e}")),
        }
    })
}

pub(super) fn run_stop<'a>(ctx: Ctx<'a>) -> CmdFuture<'a> {
    Box::pin(async move {
        match ctx.runtime.stop_turn().await {
            Ok(()) => Outcome::Silent,
            Err(e) => Outcome::Reply(format!("! [stop] {e}")),
        }
    })
}

pub(super) fn run_resume<'a>(ctx: Ctx<'a>) -> CmdFuture<'a> {
    Box::pin(async move {
        match ctx.runtime.resume() {
            Ok(true) => Outcome::Reply("resumed".into()),
            Ok(false) => Outcome::Reply("nothing to resume (not paused)".into()),
            Err(e) => Outcome::Reply(format!("! [resume] {e}")),
        }
    })
}

/// The operator's answer to `/model` with no argument.
async fn model_status(runtime: &Runtime) -> Outcome {
    let current = runtime.config().model.clone();
    let key = match runtime.model_key_state() {
        Ok(Some((_, state))) => match state {
            ka_agent_runtime::ModelKeyState::Present => None,
            ka_agent_runtime::ModelKeyState::Missing(env) => {
                Some(format!("✗ {env} missing — ka-agent key set {env}"))
            }
            ka_agent_runtime::ModelKeyState::Keyless => None,
        },
        Ok(None) => None,
        Err(e) => return Outcome::Reply(format!("! [model] {e}")),
    };
    let mut lines = vec![match &current {
        Some(m) => format!("model: {m}"),
        None => "model: none — run `ka-agent setup`".to_string(),
    }];
    if let Some(k) = key {
        lines.push(k);
    }
    lines.push("switch: ka-agent model provider/model · list: ka-agent models".into());
    Outcome::Reply(lines.join("\n"))
}

pub(super) fn run_model<'a>(ctx: Ctx<'a>) -> CmdFuture<'a> {
    Box::pin(async move {
        let runtime = ctx.runtime;
        let rest = ctx.args;
        if rest.is_empty() {
            model_status(runtime).await
        } else {
            match runtime.set_model(rest).await {
                Ok(id) => {
                    let mut reply = format!("model → {id} (next message uses it)");
                    if let Ok(Some((_, ka_agent_runtime::ModelKeyState::Missing(env)))) =
                        runtime.model_key_state()
                    {
                        reply.push_str(&format!("\n✗ {env} missing — ka-agent key set {env}"));
                    }
                    Outcome::Reply(reply)
                }
                Err(e) => Outcome::Reply(format!("! [model] {e}")),
            }
        }
    })
}

pub(super) fn run_mode<'a>(ctx: Ctx<'a>) -> CmdFuture<'a> {
    Box::pin(async move {
        let runtime = ctx.runtime;
        let rest = ctx.args;
        if rest.is_empty() {
            let current = runtime.config().mode.clone();
            return Outcome::Reply(format!(
                "mode {current} — /mode ask (asks before acting) · accept_edits (edits without asking) · allow (acts without asking) · readonly (read and plan only); the old names guarded/free/plan still work"
            ));
        }
        match runtime.set_mode(rest, ctx.source).await {
            Ok(mode) => Outcome::Reply(format!("mode → {mode}")),
            Err(e) => Outcome::Reply(format!("! [mode] {e}")),
        }
    })
}

/// The terminal client intercepts `/quit` itself; every other surface gets a
/// hint instead of an "unknown command".
pub(super) fn run_quit<'a>(_ctx: Ctx<'a>) -> CmdFuture<'a> {
    Box::pin(async move {
        Outcome::Reply("/quit leaves the terminal chat — nothing to quit here".into())
    })
}

pub(super) fn run_persona<'a>(ctx: Ctx<'a>) -> CmdFuture<'a> {
    Box::pin(async move {
        let _ = ctx.args;
        Outcome::Reply(ctx.runtime.persona_text())
    })
}

pub(super) fn run_effort<'a>(ctx: Ctx<'a>) -> CmdFuture<'a> {
    Box::pin(async move {
        match ctx.runtime.set_effort(ctx.args).await {
            Ok(reply) => Outcome::Reply(reply),
            Err(e) => Outcome::Reply(format!("! [effort] {e}")),
        }
    })
}

pub(super) fn run_undo<'a>(ctx: Ctx<'a>) -> CmdFuture<'a> {
    Box::pin(async move {
        let _ = ctx.args;
        match ctx.runtime.undo().await {
            Ok(()) => Outcome::Reply("undoing the last file change…".into()),
            Err(e) => Outcome::Reply(format!("! [undo] {e}")),
        }
    })
}

pub(super) fn run_steer<'a>(ctx: Ctx<'a>) -> CmdFuture<'a> {
    Box::pin(async move {
        let text = ctx.args.trim();
        if text.is_empty() {
            return Outcome::Reply("usage: /steer <text>".into());
        }
        match ctx.runtime.steer(text).await {
            Ok(true) => Outcome::Silent,
            Ok(false) => match ctx.runtime.handle_message(text, ctx.source).await {
                Ok(()) => Outcome::Silent,
                Err(e) => Outcome::Reply(format!("! [steer] {e}")),
            },
            Err(e) => Outcome::Reply(format!("! [steer] {e}")),
        }
    })
}

pub(super) fn run_queue<'a>(ctx: Ctx<'a>) -> CmdFuture<'a> {
    Box::pin(async move {
        if ctx.args == "clear" {
            return match ctx.runtime.clear_queue() {
                Ok(0) => Outcome::Reply("the queue is empty".into()),
                Ok(n) => Outcome::Reply(format!("cleared {n} queued message(s)")),
                Err(e) => Outcome::Reply(format!("! [queue] {e}")),
            };
        }
        match ctx.runtime.queued() {
            Ok(items) if items.is_empty() => Outcome::Reply("the queue is empty".into()),
            Ok(items) => {
                let mut out = format!("{} queued:\n", items.len());
                for (i, (id, source, text)) in items.iter().enumerate() {
                    let head: String = text.chars().take(80).collect();
                    out.push_str(&format!(
                        "  #{} [{}] {} · {}\n",
                        i + 1,
                        source,
                        ka_agent_core::ids::short_id(id),
                        head
                    ));
                }
                Outcome::Reply(out.trim_end().to_string())
            }
            Err(e) => Outcome::Reply(format!("! [queue] {e}")),
        }
    })
}

pub(super) fn run_tools<'a>(ctx: Ctx<'a>) -> CmdFuture<'a> {
    Box::pin(async move {
        let tools = ctx.runtime.tools();
        if tools.is_empty() {
            return Outcome::Reply(
                "no tools discovered yet — they appear after the first turn".into(),
            );
        }
        let mut engine: Vec<String> = Vec::new();
        let mut kagent: Vec<String> = Vec::new();
        let mut servers: std::collections::BTreeMap<String, Vec<String>> = Default::default();
        for t in tools {
            if let Some(rest) = t.strip_prefix("kagent__") {
                kagent.push(rest.to_string());
            } else if let Some((server, _tool)) = t.split_once("__") {
                servers.entry(server.to_string()).or_default().push(t);
            } else {
                engine.push(t);
            }
        }
        let mut out = format!("engine: {}", engine.join(", "));
        out.push_str(&format!("\nkagent: {}", kagent.join(", ")));
        for (server, list) in &servers {
            out.push_str(&format!("\n{server}: {}", list.join(", ")));
        }
        Outcome::Reply(out)
    })
}
