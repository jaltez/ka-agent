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
                "mode {current} — /mode guarded (asks before acting) · accept_edits (edits without asking) · plan (read and plan only) · free (acts without asking)"
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
