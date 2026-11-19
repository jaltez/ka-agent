//! Session-level commands: status, new session, stop, resume, model, mode,
//! quit.

use super::{CmdFuture, Ctx, Outcome};
use ka_agent_core::ids::short_id;
use ka_agent_runtime::Runtime;

pub(super) fn run_status<'a>(ctx: Ctx<'a>) -> CmdFuture<'a> {
    Box::pin(async move {
        match ctx.runtime.status_report_for(&ctx.principal) {
            Ok(report) => Outcome::Reply(report.render(&ctx.runtime.now_zoned())),
            Err(e) => Outcome::Reply(format!("! [status] {e}")),
        }
    })
}

pub(super) fn run_new<'a>(ctx: Ctx<'a>) -> CmdFuture<'a> {
    Box::pin(async move {
        let arg = ctx.args.trim();
        let private = match arg {
            "" => false,
            "private" | "incognito" => true,
            other => {
                return Outcome::Reply(format!("usage: /new [private] — unknown option {other:?}"));
            }
        };
        match ctx.runtime.new_session(&ctx.principal, private).await {
            Ok(id) if private => Outcome::Reply(format!(
                "🕶 private session {} — this conversation won't be remembered: no history, memory, skills or attachments; nothing it schedules outlives it",
                short_id(id.as_str())
            )),
            Ok(id) => Outcome::Reply(format!("new session {}", short_id(id.as_str()))),
            Err(e) => Outcome::Reply(format!("! [session] {e}")),
        }
    })
}

pub(super) fn run_stop<'a>(ctx: Ctx<'a>) -> CmdFuture<'a> {
    Box::pin(async move {
        match ctx.runtime.stop_turn(&ctx.principal, ctx.source).await {
            Ok(()) => Outcome::Silent,
            Err(e) => Outcome::Reply(format!("! [stop] {e}")),
        }
    })
}

pub(super) fn run_resume<'a>(ctx: Ctx<'a>) -> CmdFuture<'a> {
    Box::pin(async move {
        match ctx.runtime.resume_for(&ctx.principal).await {
            Ok(true) => Outcome::Reply("resumed".into()),
            Ok(false) => Outcome::Reply("nothing to resume (not paused)".into()),
            Err(e) => Outcome::Reply(format!("! [resume] {e}")),
        }
    })
}

/// `/retry`: re-send the newest turn when it ended in error/interrupted.
pub(super) fn run_retry<'a>(ctx: Ctx<'a>) -> CmdFuture<'a> {
    Box::pin(async move {
        match ctx.runtime.retry_last(&ctx.principal).await {
            Ok(Some(text)) => {
                let head: String = text.lines().next().unwrap_or("").chars().take(60).collect();
                Outcome::Reply(format!("↻ re-sending \"{head}\""))
            }
            Ok(None) => Outcome::Reply("nothing to retry (the last turn did not fail)".into()),
            Err(e) => Outcome::Reply(format!("! [retry] {e}")),
        }
    })
}

/// `/rewind [n]`: list this session's shadow-git checkpoints (newest
/// first) or restore one. Code only — the reply carries the original
/// question and inbound text so the operator can re-propose.
pub(super) fn run_rewind<'a>(ctx: Ctx<'a>) -> CmdFuture<'a> {
    Box::pin(async move {
        let arg = ctx.args.trim();
        let n = if arg.is_empty() {
            None
        } else {
            match arg.parse::<usize>() {
                Ok(n) if n >= 1 => Some(n),
                _ => {
                    return Outcome::Reply(
                        "usage: /rewind [n] — n is a number from the list".into(),
                    );
                }
            }
        };
        match ctx.runtime.rewind(&ctx.principal, n).await {
            Ok(text) => Outcome::Reply(text),
            Err(e) => Outcome::Reply(format!("! [rewind] {e}")),
        }
    })
}

/// The modes `/mode` offers, in pick order (`/mode <n>` resolves here).
pub const MODES: &[(&str, &str)] = &[
    ("ask", "asks before acting"),
    ("accept_edits", "edits without asking"),
    ("allow", "acts without asking"),
    ("readonly", "read and plan only"),
];

/// The reasoning-effort levels `/effort` offers, in pick order.
pub const EFFORTS: &[&str] = &["off", "low", "medium", "high", "max"];

/// The `/model` header: the current model and its key state. Shared by
/// every surface's picker (Telegram builds its keyboard under it).
pub async fn model_header(runtime: &Runtime, who: &ka_agent_core::Principal) -> String {
    let current = runtime.model_for(who, false);
    let key = match runtime.model_key_state() {
        Ok(Some((_, state))) => match state {
            ka_agent_runtime::ModelKeyState::Present => None,
            ka_agent_runtime::ModelKeyState::Missing(env) => {
                Some(format!("✗ {env} missing — ka-agent key set {env}"))
            }
            ka_agent_runtime::ModelKeyState::Keyless => None,
        },
        Ok(None) => None,
        Err(e) => return format!("! [model] {e}"),
    };
    let mut lines = vec![match &current {
        Some(m) => format!("model: {m}"),
        None => "model: none — run `ka-agent setup`".to_string(),
    }];
    if let Some(k) = key {
        lines.push(k);
    }
    lines.join("\n")
}

/// `/model` with no argument: the current model plus every provider
/// that has models to offer, numbered (`/model <n|provider>` opens one).
/// The listing filtered to what `who` may actually use: a non-admin sees
/// only the models their groups allow (an empty allowlist = the catalog).
fn allowed_listing(
    runtime: &Runtime,
    who: &ka_agent_core::Principal,
) -> ka_agent_runtime::registry::ModelsListing {
    let mut listing = runtime.models_listing();
    let (allow, _) = runtime
        .store()
        .model_policy_for(&who.username)
        .unwrap_or_default();
    if allow.is_empty() {
        return listing;
    }
    for provider in &mut listing.providers {
        provider
            .models
            .retain(|m| ka_agent_store::model_allowed(&allow, m));
    }
    listing.providers.retain(|p| !p.models.is_empty());
    listing
}

pub async fn model_picker(runtime: &Runtime, who: &ka_agent_core::Principal) -> String {
    let listing = allowed_listing(runtime, who);
    let current_vendor = listing.current_vendor().map(str::to_string);
    let mut lines = vec![model_header(runtime, who).await];
    for (i, p) in listing.providers.iter().enumerate() {
        let mark = if Some(p.name.as_str()) == current_vendor.as_deref() {
            " ←"
        } else {
            ""
        };
        let n = if p.models.len() == 1 {
            "1 model".to_string()
        } else {
            format!("{} models", p.models.len())
        };
        lines.push(format!("{}. {} — {n}{mark}", i + 1, p.name));
    }
    if listing.providers.is_empty() {
        lines.push("no models known — ka-agent models --refresh pulls the live list".into());
    }
    lines.push("/model <n|provider> lists its models · /model provider/model switches".into());
    lines.join("\n")
}

/// `/model <provider>`: that provider's models, numbered over the full
/// list — the display is capped, the numbers still reach the tail.
pub async fn model_vendor_listing(
    runtime: &Runtime,
    vendor: &str,
    who: &ka_agent_core::Principal,
) -> String {
    let reg = runtime.model_registry();
    let listing = allowed_listing(runtime, who);
    let Some(p) = listing.providers.iter().find(|p| p.name == vendor) else {
        if ka_agent_runtime::catalog::provider(vendor).is_some() {
            return format!("no models listed for {vendor} — type the id: /model {vendor}/<model>");
        }
        return format!("unknown provider {vendor:?} — /model lists them");
    };
    let current = runtime.model_for(who, false);
    let n = if p.models.len() == 1 {
        "1 model".to_string()
    } else {
        format!("{} models", p.models.len())
    };
    let mut lines = vec![format!(
        "{} — {n} · {}",
        p.name,
        ka_agent_runtime::registry::footer(reg.as_ref(), vendor)
    )];
    const SHOW: usize = 20;
    for (i, m) in p.models.iter().enumerate().take(SHOW) {
        let mark = if Some(m.as_str()) == current.as_deref() {
            " ←"
        } else {
            ""
        };
        lines.push(format!("  {}. {m}{mark}", i + 1));
    }
    if p.models.len() > SHOW {
        lines.push(format!(
            "  +{} more — /model {vendor} <n> still picks them",
            p.models.len() - SHOW
        ));
    }
    lines.push(format!(
        "/model {vendor} <n> switches · /model lists providers"
    ));
    lines.join("\n")
}

/// Resolve a provider argument: its name or its number in the listing.
fn resolve_vendor_in(
    listing: &ka_agent_runtime::registry::ModelsListing,
    arg: &str,
) -> Option<String> {
    if let Some(p) = listing.providers.iter().find(|p| p.name == arg) {
        return Some(p.name.clone());
    }
    if let Ok(n) = arg.parse::<usize>() {
        return listing
            .providers
            .get(n.wrapping_sub(1))
            .map(|p| p.name.clone());
    }
    None
}

/// `/model <provider> <n>`: the n-th model of the provider's full list.
/// `Err` carries the operator-facing reason (unknown provider, option
/// out of range) — a bad number must not silently fall back to a list.
fn model_pick(
    listing: &ka_agent_runtime::registry::ModelsListing,
    vendor_arg: &str,
    n: usize,
) -> Result<String, String> {
    let vendor = resolve_vendor_in(listing, vendor_arg)
        .ok_or_else(|| format!("unknown provider {vendor_arg:?} — /model lists them"))?;
    let provider = listing
        .providers
        .iter()
        .find(|p| p.name == vendor)
        .unwrap_or_else(|| unreachable!("resolve_vendor_in returned a listed provider"));
    provider
        .models
        .get(n.wrapping_sub(1))
        .cloned()
        .ok_or_else(|| format!("no model {n} for {vendor} (1..={})", provider.models.len()))
}

/// The switch path every pick funnels into (selector validation, live
/// reload, the missing-key warning).
async fn switch_model(runtime: &Runtime, sel: &str, who: &ka_agent_core::Principal) -> Outcome {
    match runtime.set_model(sel, who).await {
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

pub(super) fn run_model<'a>(ctx: Ctx<'a>) -> CmdFuture<'a> {
    Box::pin(async move {
        let runtime = ctx.runtime;
        let rest = ctx.args.trim();
        if rest.is_empty() {
            return Outcome::Reply(model_picker(runtime, &ctx.principal).await);
        }
        let mut words = rest.split_whitespace();
        let first = words.next().unwrap_or_default();
        // `/model <provider> <n>` (the provider may be its number too):
        // pick the n-th model of the full list, or say why not.
        if let Some(second) = words.next()
            && words.next().is_none()
            && !first.contains('/')
            && let Ok(n) = second.parse::<usize>()
        {
            let listing = allowed_listing(runtime, &ctx.principal);
            return match model_pick(&listing, first, n) {
                Ok(sel) => switch_model(runtime, &sel, &ctx.principal).await,
                Err(e) => Outcome::Reply(format!("! [model] {e}")),
            };
        }
        // `/model <provider>` (or its number): the provider's models.
        if !first.contains('/') {
            let listing = allowed_listing(runtime, &ctx.principal);
            if let Some(vendor) = resolve_vendor_in(&listing, first) {
                return Outcome::Reply(
                    model_vendor_listing(runtime, &vendor, &ctx.principal).await,
                );
            }
        }
        switch_model(runtime, rest, &ctx.principal).await
    })
}

pub(super) fn run_mode<'a>(ctx: Ctx<'a>) -> CmdFuture<'a> {
    Box::pin(async move {
        let runtime = ctx.runtime;
        let rest = ctx.args.trim();
        if rest.is_empty() {
            let current = runtime.effective_mode_for(&ctx.principal.username);
            let mut lines = vec![format!("mode: {current}")];
            for (i, (name, desc)) in MODES.iter().enumerate() {
                let mark = if *name == current { " ←" } else { "" };
                lines.push(format!("{}. {name} — {desc}{mark}", i + 1));
            }
            lines.push(
                "/mode <n|name> switches · the old names guarded/free/plan still work".into(),
            );
            return Outcome::Reply(lines.join("\n"));
        }
        // `/mode <n>`: pick by number.
        let arg = match rest.parse::<usize>() {
            Ok(n) => match MODES.get(n.wrapping_sub(1)) {
                Some((name, _)) => (*name).to_string(),
                None => {
                    return Outcome::Reply(format!("! [mode] no option {n} (1..={})", MODES.len()));
                }
            },
            Err(_) => rest.to_string(),
        };
        match runtime.set_mode(&arg, ctx.source, &ctx.principal).await {
            Ok(mode) => Outcome::Reply(format!("mode → {mode}")),
            Err(e) => Outcome::Reply(format!("! [mode] {e}")),
        }
    })
}

/// `/agent [name]`: run this conversation as a shared agent.
pub(super) fn run_agent<'a>(ctx: Ctx<'a>) -> CmdFuture<'a> {
    Box::pin(async move {
        let name = ctx.args.trim();
        match ctx
            .runtime
            .set_agent(&ctx.principal, (!name.is_empty()).then_some(name))
        {
            Ok(line) => Outcome::Reply(line),
            Err(e) => Outcome::Reply(format!("! [agent] {e}")),
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
        let runtime = ctx.runtime;
        let rest = ctx.args.trim();
        if rest.is_empty() {
            let current = runtime
                .effort(&ctx.principal)
                .unwrap_or_else(|| "model default".into());
            let mut lines = vec![format!("effort: {current}")];
            for (i, level) in EFFORTS.iter().enumerate() {
                let mark = if *level == current { " ←" } else { "" };
                lines.push(format!("{}. {level}{mark}", i + 1));
            }
            lines.push("/effort <n|level> switches (this session)".into());
            return Outcome::Reply(lines.join("\n"));
        }
        // `/effort <n>`: pick by number.
        let arg = match rest.parse::<usize>() {
            Ok(n) => match EFFORTS.get(n.wrapping_sub(1)) {
                Some(level) => (*level).to_string(),
                None => {
                    return Outcome::Reply(format!(
                        "! [effort] no option {n} (1..={})",
                        EFFORTS.len()
                    ));
                }
            },
            Err(_) => rest.to_string(),
        };
        match runtime.set_effort(&arg, &ctx.principal).await {
            Ok(reply) => Outcome::Reply(reply),
            Err(e) => Outcome::Reply(format!("! [effort] {e}")),
        }
    })
}

pub(super) fn run_undo<'a>(ctx: Ctx<'a>) -> CmdFuture<'a> {
    Box::pin(async move {
        let _ = ctx.args;
        match ctx.runtime.undo(&ctx.principal).await {
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
        match ctx.runtime.steer(text, &ctx.principal).await {
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
        let args = ctx.args.trim();
        if args.is_empty() || args == "clear" {
            if args == "clear" {
                return match ctx.runtime.clear_queue(&ctx.principal) {
                    Ok(0) => Outcome::Reply("the queue is empty".into()),
                    Ok(n) => Outcome::Reply(format!("cleared {n} queued message(s)")),
                    Err(e) => Outcome::Reply(format!("! [queue] {e}")),
                };
            }
            return match ctx.runtime.queued(&ctx.principal) {
                Ok(items) if items.is_empty() => Outcome::Reply("the queue is empty".into()),
                Ok(items) => Outcome::Reply(render_queue(&items)),
                Err(e) => Outcome::Reply(format!("! [queue] {e}")),
            };
        }
        // `rm <n> · edit <n> <text> · top <n> · up|down <n> · now <n>`: the
        // position is resolved against `queued()` here, so every surface
        // speaks `#n` while the runtime works with ids.
        let (sub, rest) = args.split_once(char::is_whitespace).unwrap_or((args, ""));
        let rest = rest.trim();
        let runtime = ctx.runtime;
        let who = &ctx.principal;
        match sub {
            "rm" | "remove" => {
                if rest.is_empty() {
                    return Outcome::Reply("usage: /queue rm <n>".into());
                }
                match runtime.remove_queued(rest, who) {
                    Ok(true) => Outcome::Reply("removed".into()),
                    Ok(false) => Outcome::Reply("! [queue] no queued message matches".into()),
                    Err(e) => Outcome::Reply(format!("! [queue] {e}")),
                }
            }
            "edit" => match rest.split_once(char::is_whitespace) {
                None => Outcome::Reply("usage: /queue edit <n> <text>".into()),
                Some((n, text)) => match runtime.edit_queued(n, text.trim(), who) {
                    Ok(true) => Outcome::Reply("edited".into()),
                    Ok(false) => Outcome::Reply("! [queue] no queued message matches".into()),
                    Err(e) => Outcome::Reply(format!("! [queue] {e}")),
                },
            },
            "top" => queue_move_reply(ctx, rest, 1).await,
            "up" | "down" => {
                // One step up/down from the item's current position.
                match queue_position(runtime, who, rest) {
                    Ok(pos) => {
                        let len = runtime.queued(who).map(|q| q.len()).unwrap_or(0);
                        let to = if sub == "up" {
                            pos.saturating_sub(1).max(1)
                        } else {
                            (pos + 1).min(len)
                        };
                        queue_move_reply(ctx, rest, to).await
                    }
                    Err(e) => Outcome::Reply(format!("! [queue] {e}")),
                }
            }
            "now" => match runtime.send_queued_now(rest, who).await {
                Ok(Some(text)) => {
                    let head: String = text.chars().take(60).collect();
                    Outcome::Reply(format!("⏭ promoted — starting now: {head}"))
                }
                Ok(None) => Outcome::Reply("! [queue] no queued message matches".into()),
                Err(e) => Outcome::Reply(format!("! [queue] {e}")),
            },
            _ => Outcome::Reply(
                "usage: /queue [clear|rm <n>|edit <n> <text>|top <n>|up <n>|down <n>|now <n>]"
                    .into(),
            ),
        }
    })
}

/// The shared `/queue` listing.
fn render_queue(items: &[(String, String, String)]) -> String {
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
    out.trim_end().to_string()
}

/// Resolve `/queue`'s 1-based position (or an id prefix) to `#n`.
fn queue_position(
    runtime: &Runtime,
    who: &ka_agent_core::Principal,
    key: &str,
) -> Result<usize, String> {
    let items = runtime.queued(who).map_err(|e| format!("! [queue] {e}"))?;
    let key = key.trim();
    if let Ok(n) = key.parse::<usize>() {
        if n == 0 || n > items.len() {
            return Err(format!("! [queue] no queued message #{key}"));
        }
        return Ok(n);
    }
    items
        .iter()
        .position(|(id, _, _)| ka_agent_core::ids::id_matches(id, key))
        .map(|i| i + 1)
        .ok_or_else(|| format!("! [queue] no queued message matches {key:?}"))
}

/// `top|up|down <n>`: move and reply with the refreshed list.
async fn queue_move_reply(ctx: Ctx<'_>, key: &str, to: usize) -> Outcome {
    match ctx.runtime.reorder_queued(key, to, &ctx.principal) {
        Ok(true) => match ctx.runtime.queued(&ctx.principal) {
            Ok(items) => Outcome::Reply(render_queue(&items)),
            Err(e) => Outcome::Reply(format!("! [queue] {e}")),
        },
        Ok(false) => Outcome::Reply("! [queue] no queued message matches".into()),
        Err(e) => Outcome::Reply(format!("! [queue] {e}")),
    }
}

pub(super) fn run_tools<'a>(ctx: Ctx<'a>) -> CmdFuture<'a> {
    Box::pin(async move {
        let tools = ctx.runtime.tools(&ctx.principal);
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

/// The slots `/preset` offers.
const PRESET_SLOTS: u8 = 3;

/// `/preset` — numbered model+mode+effort slots: `/preset` lists them,
/// `/preset save <1-3>` captures the live triple, `/preset <n>` applies a
/// slot through the existing setters (model ACLs and role mode ceilings
/// are inherited — a preset can never lift either).
pub(super) fn run_preset<'a>(ctx: Ctx<'a>) -> CmdFuture<'a> {
    Box::pin(async move {
        let runtime = ctx.runtime;
        let who = &ctx.principal;
        let store = runtime.store();
        let rest = ctx.args.trim();
        if rest.is_empty() {
            let saved = store.list_presets(&who.username).unwrap_or_default();
            let mut lines = vec!["presets:".to_string()];
            for slot in 1..=PRESET_SLOTS {
                let line = match saved.iter().find(|p| p.slot == slot) {
                    Some(p) => format!(
                        "{}. {} · mode {} · effort {}",
                        slot,
                        p.model.as_deref().unwrap_or("(config model)"),
                        p.mode.as_deref().unwrap_or("(profile default)"),
                        p.effort.as_deref().unwrap_or("(model default)"),
                    ),
                    None => format!("{slot}. (empty)"),
                };
                lines.push(line);
            }
            lines.push("/preset save <1-3> captures the current model+mode+effort · /preset <n> applies one".into());
            return Outcome::Reply(lines.join("\n"));
        }
        let (sub, rest) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
        let rest = rest.trim();
        let slot = match rest.parse::<u8>() {
            Ok(n) if (1..=PRESET_SLOTS).contains(&n) => n,
            _ => {
                return Outcome::Reply(format!(
                    "usage: /preset | /preset <1-{PRESET_SLOTS}> | /preset save <1-{PRESET_SLOTS}>"
                ));
            }
        };
        if sub == "save" {
            let model = runtime.model_for(who, false);
            let mode = runtime.effective_mode_for(&who.username);
            let effort = runtime.effort(who);
            match store.save_preset(
                &who.username,
                slot,
                model.as_deref(),
                Some(mode.as_str()),
                effort.as_deref(),
            ) {
                Ok(()) => Outcome::Reply(format!(
                    "preset {slot} saved: {} · mode {mode} · effort {}",
                    model.as_deref().unwrap_or("(config model)"),
                    effort.as_deref().unwrap_or("model default"),
                )),
                Err(e) => Outcome::Reply(format!("! [preset] {e}")),
            }
        } else {
            let Some(p) = store.preset(&who.username, slot).unwrap_or(None) else {
                return Outcome::Reply(format!(
                    "! [preset] slot {slot} is empty — /preset save {slot} captures the current settings"
                ));
            };
            // Apply through the existing setters: each enforces its own
            // policy (model ACL, role mode ceiling), so the reply names
            // the same refusal `/model` or `/mode` would give.
            let mut steps: Vec<String> = Vec::new();
            if let Some(sel) = &p.model {
                match runtime.set_model(sel, who).await {
                    Ok(id) => steps.push(format!("model → {id}")),
                    Err(e) => return Outcome::Reply(format!("! [preset] model: {e}")),
                }
            }
            if let Some(mode) = &p.mode {
                match runtime.set_mode(mode, ctx.source, who).await {
                    Ok(m) => steps.push(format!("mode → {m}")),
                    Err(e) => return Outcome::Reply(format!("! [preset] mode: {e}")),
                }
            }
            if let Some(level) = &p.effort {
                match runtime.set_effort(level, who).await {
                    Ok(reply) => steps.push(reply),
                    Err(e) => return Outcome::Reply(format!("! [preset] effort: {e}")),
                }
            }
            if steps.is_empty() {
                steps.push("nothing to apply (the slot holds no overrides)".into());
            }
            Outcome::Reply(format!("preset {slot} → {}", steps.join(" · ")))
        }
    })
}

/// `/archive` — hide this session from the lists without touching its
/// transcript: `/archive` archives the caller's live session, `/archive
/// list` shows archived ones, `/archive undo <id-prefix>` restores.
pub(super) fn run_archive<'a>(ctx: Ctx<'a>) -> CmdFuture<'a> {
    Box::pin(async move {
        let runtime = ctx.runtime;
        let who = &ctx.principal;
        let store = runtime.store();
        let rest = ctx.args.trim();
        if rest.is_empty() {
            // The caller's live session: one command, no prompt.
            let report = match runtime.status_report_for(who) {
                Ok(r) => r,
                Err(e) => return Outcome::Reply(format!("! [archive] {e}")),
            };
            let sid = ka_agent_core::SessionId(report.session_id.clone());
            return match store.set_session_archived(&sid, true) {
                Ok(()) => Outcome::Reply(format!(
                    "🗄 session {} archived — hidden from the lists, its transcript is kept. /new starts a fresh session; /archive undo {} restores",
                    short_id(sid.as_str()),
                    short_id(sid.as_str()),
                )),
                Err(e) => Outcome::Reply(format!("! [archive] {e}")),
            };
        }
        let (sub, rest) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
        let rest = rest.trim();
        match sub {
            "list" => {
                let mine = store
                    .sessions_for_user(&who.username)
                    .unwrap_or_default()
                    .into_iter()
                    .filter(|s| s.archived)
                    .collect::<Vec<_>>();
                if mine.is_empty() {
                    return Outcome::Reply("no archived sessions".into());
                }
                let mut out = format!("{} archived:\n", mine.len());
                for s in &mine {
                    out.push_str(&format!(
                        "  {} · updated {}\n",
                        short_id(s.id.as_str()),
                        ka_agent_core::when::describe(s.updated_ms, &runtime.now_zoned()),
                    ));
                }
                Outcome::Reply(out.trim_end().to_string())
            }
            "undo" => {
                if rest.is_empty() {
                    return Outcome::Reply("usage: /archive undo <id-prefix>".into());
                }
                let Some(s) = store.find_session_by_prefix(rest).unwrap_or(None) else {
                    return Outcome::Reply(format!("! [archive] no session matches {rest:?}"));
                };
                // Another user's session id prefix is refused.
                if s.username != who.username {
                    return Outcome::Reply("! [archive] no session of yours matches".into());
                }
                match store.set_session_archived(&s.id, false) {
                    Ok(()) => Outcome::Reply(format!(
                        "session {} restored to the lists",
                        short_id(s.id.as_str())
                    )),
                    Err(e) => Outcome::Reply(format!("! [archive] {e}")),
                }
            }
            _ => {
                Outcome::Reply("usage: /archive | /archive list | /archive undo <id-prefix>".into())
            }
        }
    })
}
