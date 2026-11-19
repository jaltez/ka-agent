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
        let enabled = ctx.runtime.skills_enabled(&ctx.principal);
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
                match ctx
                    .runtime
                    .handle_skill(name, prompt, ctx.source, &ctx.principal)
                    .await
                {
                    // The turn itself streams back through the surface.
                    Ok(()) => Outcome::Silent,
                    Err(e) => Outcome::Reply(format!("! [skill] {e}")),
                }
            }
            _ => {
                // No usable arguments: list what a `/skill <name> <text>`
                // turn can load (the same inventory `/skills` shows).
                let enabled = ctx.runtime.skills_enabled(&ctx.principal);
                if enabled.is_empty() {
                    return Outcome::Reply(
                        "no skills enabled — `ka-agent skills enable <name>` provisions one".into(),
                    );
                }
                let mut lines =
                    vec!["one turn with a skill loaded: /skill <name> <text>".to_string()];
                for s in enabled {
                    if s.description.is_empty() {
                        lines.push(format!("  {}", s.name));
                    } else {
                        lines.push(format!("  {} — {}", s.name, s.description));
                    }
                }
                Outcome::Reply(lines.join("\n"))
            }
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
            match ctx.runtime.enable_skill(&one.name, &ctx.principal) {
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

/// `/skill-doctor`: per-skill context cost and attach counts. The engine
/// assembles the prompt, so token figures are labeled estimates (bytes/4).
pub(super) fn run_skill_doctor<'a>(ctx: Ctx<'a>) -> CmdFuture<'a> {
    Box::pin(async move {
        let resolved = ctx.runtime.skills_resolved(&ctx.principal);
        if resolved.is_empty() {
            return Outcome::Reply(
                "no skills enabled — `ka-agent skills enable <name>` provisions one".into(),
            );
        }
        let usage = ctx
            .runtime
            .store()
            .skill_usage(&ctx.principal.username)
            .unwrap_or_default();
        let by_name: std::collections::BTreeMap<&str, &ka_agent_store::SkillUsage> =
            usage.iter().map(|row| (row.skill.as_str(), row)).collect();
        let now = ctx.runtime.now_zoned();
        let mut lines =
            vec!["skills — context cost (estimates; the engine assembles the prompt)".to_string()];
        let mut total_tokens = 0u64;
        let mut total_bytes = 0u64;
        for skill in resolved {
            let info = &skill.info;
            let advertised = info.name.len() + info.description.len();
            let tokens = (advertised as u64).div_ceil(4);
            let kib = skill.body_bytes.div_ceil(1024);
            total_tokens += tokens;
            total_bytes += skill.body_bytes;
            let mut line = format!(
                "  {}  ~{} tok advertised · {}KiB body",
                info.name, tokens, kib
            );
            match by_name.get(info.name.as_str()) {
                Some(row) => {
                    let mut kinds = Vec::new();
                    if row.explicit > 0 {
                        kinds.push(format!("{}× (explicit)", row.explicit));
                    }
                    if row.trigger > 0 {
                        kinds.push(format!("{}× (trigger)", row.trigger));
                    }
                    if row.paths > 0 {
                        kinds.push(format!("{}× (paths)", row.paths));
                    }
                    if !kinds.is_empty() {
                        line.push_str(&format!(" · {}", kinds.join(", ")));
                    }
                    line.push_str(&format!(
                        " · last used {}",
                        super::local_compact(row.last_used_ms, &now)
                    ));
                }
                None => line.push_str(" · never attached"),
            }
            if !info.triggers.is_empty() {
                line.push_str(&format!(" · triggers: {}", info.triggers.join(", ")));
            }
            if !info.paths.is_empty() {
                line.push_str(&format!(" · paths: {}", info.paths.join(", ")));
            }
            if !skill.source_copy {
                line.push_str(" · no source copy");
            }
            lines.push(line);
        }
        lines.push(format!(
            "standing: ~{} tok advertised · {}KiB body",
            total_tokens,
            total_bytes.div_ceil(1024)
        ));
        Outcome::Reply(lines.join("\n"))
    })
}
