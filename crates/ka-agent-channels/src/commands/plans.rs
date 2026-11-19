//! Plan commands (v4 F12): the editable plan artifact — draft, list,
//! show, set, approve, run, stop. The artifact and its status machine
//! live in `docs/contract/plans.md`; the runtime's capture and the
//! verify/repair cycle sit in the `Idle` boundary (`runtime/events.rs`).
//!
//! Security line: `verify` is a shell command the daemon runs — it arms
//! only through `/plan approve`, which is operator-only, and approval
//! consents to exactly the lifted `Verify:` string (`/plan show` renders
//! it unarmed until then).

use std::sync::atomic::{AtomicU64, Ordering};

use super::{CmdFuture, Ctx, Outcome};
use ka_agent_core::ids::{id_matches, short_id};
use ka_agent_store::{PlanRow, PlanStatus};

pub(super) fn run_plan<'a>(ctx: Ctx<'a>) -> CmdFuture<'a> {
    Box::pin(async move {
        let (verb, rest) = match ctx.args.split_once(' ') {
            Some((v, r)) => (v, r.trim()),
            None => (ctx.args, ""),
        };
        match verb {
            "draft" if !rest.is_empty() => draft(ctx, rest).await,
            "list" | "" => list(ctx),
            "show" => show(ctx, rest),
            "set" => set(ctx, rest),
            "approve" => approve(ctx, rest),
            "run" => run(ctx, rest).await,
            "stop" => stop(ctx, rest),
            "draft" => Outcome::Reply("usage: /plan draft <goal>".into()),
            other => Outcome::Reply(format!(
                "unknown /plan verb {other:?} — draft | list | show | set | approve | run | stop"
            )),
        }
    })
}

/// Mint a plan id in the house shape (`<prefix>_<hex time>-<hex counter>-
/// <hex entropy>`; `ids.rs` keeps its generator private, so this mirrors
/// it locally — same uniqueness construction, different prefix).
fn new_plan_id() -> String {
    use std::hash::{BuildHasher, Hasher};
    static CTR: AtomicU64 = AtomicU64::new(0);
    let n = CTR.fetch_add(1, Ordering::Relaxed);
    let t = ka_agent_core::now_ms().max(0) as u64;
    let entropy = std::collections::hash_map::RandomState::new()
        .build_hasher()
        .finish();
    format!("plan_{t:x}-{n:02x}-{entropy:08x}")
}

fn session_of(ctx: &Ctx<'_>) -> Result<ka_agent_core::SessionId, Outcome> {
    ctx.runtime.session_id_for(&ctx.principal).ok_or_else(|| {
        Outcome::Reply("! [plan] no live session — send a message first".to_string())
    })
}

/// Resolve a `/plan <verb> <id>` prefix against this user's plans (a
/// plan survives session rotation; `session` is provenance).
fn resolve(ctx: &Ctx<'_>, prefix: &str) -> Result<PlanRow, Outcome> {
    let store = ctx.runtime.store();
    let rows = store
        .plans_for_user(&ctx.principal.username)
        .map_err(|e| Outcome::Reply(format!("! [plan] {e}")))?;
    let hits: Vec<&PlanRow> = rows.iter().filter(|p| id_matches(&p.id, prefix)).collect();
    match hits.as_slice() {
        [one] => Ok((*one).clone()),
        [] => Err(Outcome::Reply(format!(
            "no plan matches {prefix:?} — /plan list shows your plans"
        ))),
        _ => Err(Outcome::Reply(format!(
            "ambiguous plan id {prefix:?} — /plan list to disambiguate"
        ))),
    }
}

fn title_of(goal: &str) -> String {
    let first = goal.lines().next().unwrap_or("").trim();
    first.chars().take(60).collect()
}

async fn draft(ctx: Ctx<'_>, goal: &str) -> Outcome {
    let sid = match session_of(&ctx) {
        Ok(s) => s,
        Err(o) => return o,
    };
    let id = new_plan_id();
    let row = PlanRow {
        id: id.clone(),
        session: sid.clone(),
        username: ctx.principal.username.clone(),
        title: title_of(goal),
        body: String::new(),
        verify: None,
        max_iterations: 3,
        iterations: 0,
        status: PlanStatus::Drafting,
        draft_turn: None,
        created_ms: ka_agent_core::now_ms(),
        updated_ms: ka_agent_core::now_ms(),
    };
    if let Err(e) = ctx.runtime.store().insert_plan(&row) {
        return Outcome::Reply(format!("! [plan] {e}"));
    }
    // The draft turn: its answer becomes the body at the Idle boundary
    // (capture lifts the `Verify:` line and flips drafting -> draft).
    let prompt = format!(
        "Write the execution plan for the goal below. Reply with the plan only, \
         as markdown: a numbered list of concrete steps, then a final line \
         `Verify: <shell command>` naming the one command that mechanically \
         checks the finished work (or `Verify: none` when there is no such \
         command). You may refine the draft with the plan_update tool.\n\n\
         Plan id: {id}\n\nGoal: {goal}"
    );
    let busy = ctx.runtime.session_status_for(&ctx.principal) != ka_agent_core::SessionStatus::Idle;
    if let Err(e) = ctx
        .runtime
        .handle_message_as(&prompt, &format!("plan:{id}"), &ctx.principal)
        .await
    {
        return Outcome::Reply(format!("! [plan] draft: {e}"));
    }
    let short = short_id(&id);
    if busy {
        Outcome::Reply(format!(
            "plan {short} queued for drafting (the session is busy) — /plan show {short}"
        ))
    } else {
        Outcome::Reply(format!(
            "plan {short} drafting — its reply below becomes the draft"
        ))
    }
}

fn list(ctx: Ctx<'_>) -> Outcome {
    let rows = match ctx.runtime.store().plans_for_user(&ctx.principal.username) {
        Ok(r) => r,
        Err(e) => return Outcome::Reply(format!("! [plan] {e}")),
    };
    if rows.is_empty() {
        return Outcome::Reply("no plans yet — /plan draft <goal>".into());
    }
    let mut out = String::from("your plans:\n");
    for p in &rows {
        out.push_str(&format!(
            "  {} [{}] repairs {}/{} — {}\n",
            short_id(&p.id),
            p.status,
            p.iterations,
            p.max_iterations,
            p.title
        ));
    }
    out.push_str("\n/plan show <id> · approve is operator-only");
    Outcome::Reply(out.trim_end().to_string())
}

fn show(ctx: Ctx<'_>, prefix: &str) -> Outcome {
    if prefix.is_empty() {
        return Outcome::Reply("usage: /plan show <id>".into());
    }
    let p = match resolve(&ctx, prefix) {
        Ok(p) => p,
        Err(o) => return o,
    };
    let verify = match &p.verify {
        Some(v) => format!("verify: `{v}` — unarmed until /plan approve"),
        None => "verify: none — runs gateless (settles done on a clean turn)".to_string(),
    };
    let body = if p.body.is_empty() {
        "(body not captured yet)".to_string()
    } else {
        p.body.clone()
    };
    Outcome::Reply(format!(
        "plan {} [{}] — {}\nrepairs: {}/{}\n{verify}\n\n{body}",
        short_id(&p.id),
        p.status,
        p.title,
        p.iterations,
        p.max_iterations,
    ))
}

fn set(ctx: Ctx<'_>, rest: &str) -> Outcome {
    let Some((prefix, body)) = rest.split_once(' ') else {
        return Outcome::Reply("usage: /plan set <id> <markdown>".into());
    };
    let body = body.trim();
    if body.is_empty() {
        return Outcome::Reply("usage: /plan set <id> <markdown>".into());
    }
    let p = match resolve(&ctx, prefix) {
        Ok(p) => p,
        Err(o) => return o,
    };
    match ctx.runtime.store().revise_plan_body(&p.id, body) {
        Ok(()) => Outcome::Reply(format!(
            "plan {} rewritten — back to draft; /plan approve {} to arm it again",
            short_id(&p.id),
            short_id(&p.id)
        )),
        Err(e) => Outcome::Reply(format!("! [plan] {e}")),
    }
}

fn approve(ctx: Ctx<'_>, prefix: &str) -> Outcome {
    if !ctx.principal.is_admin() {
        return Outcome::Reply(
            "! [plan] approve is operator-only — it arms a shell command".to_string(),
        );
    }
    if prefix.is_empty() {
        return Outcome::Reply("usage: /plan approve <id>".into());
    }
    let p = match resolve(&ctx, prefix) {
        Ok(p) => p,
        Err(o) => return o,
    };
    let short = short_id(&p.id);
    match p.status {
        PlanStatus::Draft => {}
        PlanStatus::Approved => {
            return Outcome::Reply(format!("plan {short} is already approved"));
        }
        PlanStatus::Drafting => {
            return Outcome::Reply(format!(
                "plan {short} is still being drafted — wait for the capture"
            ));
        }
        PlanStatus::Executing => {
            return Outcome::Reply(format!(
                "plan {short} is executing — /plan stop {short} first"
            ));
        }
        other => {
            return Outcome::Reply(format!(
                "plan {short} is {other} — /plan set {short} rewrites it, then approve"
            ));
        }
    }
    match ctx
        .runtime
        .store()
        .transition_plan(&p.id, PlanStatus::Draft, PlanStatus::Approved)
    {
        Ok(()) => {
            let armed = p
                .verify
                .as_ref()
                .map(|v| format!("verify armed: `{v}` (runs as the daemon after each run turn)"))
                .unwrap_or_else(|| {
                    "no verify command: the run settles done on a clean turn".into()
                });
            Outcome::Reply(format!("plan {short} approved — {armed}"))
        }
        Err(e) => Outcome::Reply(format!("! [plan] {e}")),
    }
}

async fn run(ctx: Ctx<'_>, prefix: &str) -> Outcome {
    if prefix.is_empty() {
        return Outcome::Reply("usage: /plan run <id>".into());
    }
    let p = match resolve(&ctx, prefix) {
        Ok(p) => p,
        Err(o) => return o,
    };
    let short = short_id(&p.id);
    // `approved` arms the run; an `executing` plan re-runs in place —
    // the documented recovery after a restart or a budget-deferred
    // repair (boot resumes nothing; the operator re-runs /plan run).
    match p.status {
        PlanStatus::Approved => {
            if let Err(e) = ctx.runtime.store().transition_plan(
                &p.id,
                PlanStatus::Approved,
                PlanStatus::Executing,
            ) {
                return Outcome::Reply(format!("! [plan] {e}"));
            }
        }
        PlanStatus::Executing => {}
        other => {
            return Outcome::Reply(format!(
                "plan {short} is {other} — run needs an approved plan (/plan set + /plan approve to prepare)"
            ));
        }
    }
    let prompt = format!(
        "Execute the approved plan below now, step by step, with your tools \
         (plan id {}). When the work is done, summarize what changed.\n\n\
         <plan>\n{}\n</plan>",
        p.id, p.body
    );
    let busy = ctx.runtime.session_status_for(&ctx.principal) != ka_agent_core::SessionStatus::Idle;
    if let Err(e) = ctx
        .runtime
        .handle_message_as(&prompt, &format!("plan:{}", p.id), &ctx.principal)
        .await
    {
        return Outcome::Reply(format!("! [plan] run: {e}"));
    }
    if busy {
        Outcome::Reply(format!(
            "plan {short} queued for execution — repairs {}/{}",
            p.iterations, p.max_iterations
        ))
    } else {
        Outcome::Reply(format!(
            "plan {short} executing — repairs {}/{}",
            p.iterations, p.max_iterations
        ))
    }
}

fn stop(ctx: Ctx<'_>, prefix: &str) -> Outcome {
    if prefix.is_empty() {
        return Outcome::Reply("usage: /plan stop <id>".into());
    }
    let p = match resolve(&ctx, prefix) {
        Ok(p) => p,
        Err(o) => return o,
    };
    let short = short_id(&p.id);
    match p.status {
        PlanStatus::Abandoned => {
            return Outcome::Reply(format!("plan {short} is already abandoned"));
        }
        PlanStatus::Done | PlanStatus::Failed => {
            return Outcome::Reply(format!("plan {short} is already settled ({})", p.status));
        }
        _ => {}
    }
    if let Err(e) =
        ctx.runtime
            .store()
            .transition_plan(&p.id, p.status.clone(), PlanStatus::Abandoned)
    {
        return Outcome::Reply(format!("! [plan] {e}"));
    }
    // Drop the pending repair, if one is still queued (the runtime may
    // also have already drained it into a running turn — the abandoned
    // status makes that turn's boundary a no-op either way).
    let mut dropped = false;
    if let Ok(sid) = session_of(&ctx)
        && let Ok(items) = ctx.runtime.store().queued_items(&sid)
    {
        for item in items {
            if item.turned.is_none()
                && item.source == format!("plan:{}", p.id)
                && ctx.runtime.store().queue_remove(&item.id).unwrap_or(false)
            {
                dropped = true;
            }
        }
    }
    Outcome::Reply(format!(
        "plan {short} abandoned{}",
        if dropped {
            " — queued repair removed"
        } else {
            ""
        }
    ))
}
