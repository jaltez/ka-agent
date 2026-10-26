//! Skill commands: the enabled list and one skill-loaded turn.

use super::{CmdFuture, Ctx, Outcome};

pub(super) fn run_skills<'a>(ctx: Ctx<'a>) -> CmdFuture<'a> {
    Box::pin(async move {
        let enabled = ctx.runtime.skills_enabled();
        let reply = if enabled.is_empty() {
            "no skills enabled — `ka-agent skills enable <name>` provisions one".to_string()
        } else {
            enabled
                .into_iter()
                .map(|s| {
                    if s.description.is_empty() {
                        s.name
                    } else {
                        format!("{} — {}", s.name, s.description)
                    }
                })
                .collect::<Vec<_>>()
                .join("\n")
        };
        Outcome::Reply(reply)
    })
}

pub(super) fn run_skill<'a>(ctx: Ctx<'a>) -> CmdFuture<'a> {
    Box::pin(async move {
        let rest = ctx.args;
        match rest.split_once(' ') {
            Some((name, prompt)) if !name.is_empty() && !prompt.is_empty() => {
                match ctx.runtime.handle_skill(name, prompt, ctx.source).await {
                    // The turn itself streams back through the surface.
                    Ok(()) => Outcome::Silent,
                    Err(e) => Outcome::Reply(format!("! [skill] {e}")),
                }
            }
            _ => Outcome::Reply("usage: /skill <name> <text>".into()),
        }
    })
}
