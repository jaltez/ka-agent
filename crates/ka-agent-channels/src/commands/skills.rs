//! Skill commands: the enabled list and one skill-loaded turn.

use super::{CmdFuture, Ctx, Outcome};

pub(super) fn run_skills<'a>(ctx: Ctx<'a>) -> CmdFuture<'a> {
    Box::pin(async move {
        // `/skills review|accept <id>|reject <id>`: model-proposed drafts.
        let (verb, arg) = match ctx.args.split_once(' ') {
            Some((v, a)) => (v, a.trim()),
            None => (ctx.args, ""),
        };
        match verb {
            "review" | "accept" | "reject" => return skill_review(ctx, verb, arg).await,
            _ => {}
        }
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

/// The skill-proposal queue: list, accept (install) or reject.
async fn skill_review(ctx: Ctx<'_>, verb: &str, arg: &str) -> Outcome {
    use ka_agent_core::ids::{id_matches, short_id};
    let store = ctx.runtime.store();
    let pending = match store.pending_skill_candidates() {
        Ok(p) => p,
        Err(e) => return Outcome::Reply(format!("! [skills] {e}")),
    };
    match verb {
        "review" => {
            if pending.is_empty() {
                return Outcome::Reply("no skill proposals to review".into());
            }
            let mut out = String::from("skill proposals (review · accept <id> · reject <id>):\n");
            for c in &pending {
                out.push_str(&format!(
                    "  {} {} — {}\n",
                    short_id(&c.id),
                    c.name,
                    c.description
                ));
            }
            Outcome::Reply(out.trim_end().to_string())
        }
        _ => {
            let hits: Vec<_> = pending.iter().filter(|c| id_matches(&c.id, arg)).collect();
            let one = match hits.as_slice() {
                [one] => *one,
                [] => return Outcome::Reply(format!("no skill proposal matches {arg:?}")),
                _ => {
                    return Outcome::Reply(
                        "ambiguous proposal id — /skills review lists them".into(),
                    );
                }
            };
            if verb == "reject" {
                if let Err(e) = store.set_skill_candidate_status(&one.id, "rejected") {
                    return Outcome::Reply(format!("! [skills] {e}"));
                }
                return Outcome::Reply(format!("rejected skill proposal {}", one.name));
            }
            // Accept: write `<base>/skills/<name>/SKILL.md`, then enable it.
            let dir = ka_agent_core::layout::base_dir()
                .join("skills")
                .join(&one.name);
            let body = format!(
                "---\nname: {}\ndescription: {}\n---\n{}\n",
                one.name,
                one.description,
                one.body.trim_end()
            );
            if let Err(e) = std::fs::create_dir_all(&dir)
                .and_then(|_| std::fs::write(dir.join("SKILL.md"), body))
            {
                return Outcome::Reply(format!("! [skills] {}: {e}", dir.display()));
            }
            if let Err(e) = store.set_skill_candidate_status(&one.id, "accepted") {
                return Outcome::Reply(format!("! [skills] {e}"));
            }
            match ctx.runtime.enable_skill(&one.name) {
                Ok(true) => Outcome::Reply(format!(
                    "installed and enabled skill {} — /skill:{} uses it",
                    one.name, one.name
                )),
                Ok(false) => Outcome::Reply(format!(
                    "accepted skill {} (the library entry vanished before provisioning)",
                    one.name
                )),
                Err(e) => Outcome::Reply(format!(
                    "installed skill {} in the library (enable it: ka-agent skills enable {}): {e}",
                    one.name, one.name
                )),
            }
        }
    }
}
