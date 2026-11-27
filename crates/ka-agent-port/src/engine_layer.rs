//! The engine layer Ka Agent hands each worker: the operator's optional
//! `ka.engine_layer` file plus what Ka Agent's own config turns on in the
//! engine — web search (`[[search]]`), the fallback model chain
//! (`[fallback]`), the fast role (`[roles] fast`), idle recaps
//! (`[context] idle_minutes`), engine hooks (`[[hooks]]`) and operator
//! MCP servers (`[[mcp]]`) — and, per role, its posture: `[features]
//! disable`, `[features] allow`, and only the fallback models the user
//! may use. Composed as TOML text, so the worker protocol
//! (`engine_layer_toml`) is unchanged and the worker's security overlay
//! still applies last.
//!
//! The operator's file is kept as written wherever Ka Agent has nothing
//! to say; where it has — a Ka Agent key for the same table, a role
//! narrowing, an appended row — Ka Agent's entry wins. One thing is
//! refused outright: a `[permissions] profile` activation.

use ka_agent_core::DaemonConfig;

/// Prefix marking a config value that must come from the secret store.
pub const SECRET_PREFIX: &str = "secret:";

/// Compose the layer for the operator's own sessions: no role posture,
/// and no model policy (the operator is not held to one). `user_layer`
/// is the operator's engine-layer file text; `resolve_secret` maps a
/// secret name to its value (the secret store), so `secret:<name>`
/// values never touch the config file.
pub fn compose(
    cfg: &DaemonConfig,
    user_layer: Option<&str>,
    resolve_secret: &dyn Fn(&str) -> Result<String, String>,
) -> Result<Option<String>, String> {
    compose_for_role(cfg, None, user_layer, resolve_secret, &|_| true)
}

/// Whether the layer Ka Agent composes for the operator carries a fast
/// role (`[roles] fast`): the operator's own row in `user_layer`, or the
/// one composed from `background_model`. The engine runs its chores on
/// that role — idle recaps, `auto_review`, the prewalk, `/btw` — and
/// skips them without one, so `doctor` reads this before it says those
/// features are on.
pub fn has_fast_role(cfg: &DaemonConfig, user_layer: Option<&str>) -> bool {
    cfg.background_model.is_some()
        || user_layer
            .and_then(|text| text.parse::<toml::Table>().ok())
            .is_some_and(|doc| operator_fast_row(&doc))
}

/// Whether the operator's layer names its own fast role: that row is the
/// operator's choice and stays, whatever `background_model` says.
fn operator_fast_row(doc: &toml::Table) -> bool {
    doc.get("roles").and_then(|r| r.get("fast")).is_some()
}

/// The role's effective disable specs: the profile baseline plus the
/// role's own, deduplicated in order. `None` = the admin role (baseline
/// only).
fn disable_specs(
    cfg: &DaemonConfig,
    role: Option<&ka_agent_core::RoleSpec>,
) -> Result<Vec<String>, String> {
    let mut out: Vec<String> = Vec::new();
    for spec in cfg
        .tools
        .disable
        .iter()
        .chain(role.into_iter().flat_map(|r| r.disable.iter()))
    {
        if !ka_agent_core::valid_feature_spec(spec) {
            return Err(format!(
                "engine layer: feature spec {spec:?} is not valid \
                 (agents|skills|mcp|hooks|web|lsp|debug|cron|monitor|generate_image \
                 or mcp:<server>|skill:<name>|tool:<hand>|agent:<name>)"
            ));
        }
        if !out.iter().any(|s| s == spec) {
            out.push(spec.clone());
        }
    }
    // Cross-check with the engine's own parser before any spawn: grammar
    // drift fails closed here, naming the offending spec.
    if !out.is_empty() {
        ka_engine::features::parse_specs(&out).map_err(|e| format!("engine layer: {e}"))?;
    }
    Ok(out)
}

/// Compose for one role (`None` = the implicit admin role): the profile
/// baseline plus the role's own posture —
///
/// - `[features] disable`: the baseline plus the role's subtractive specs
///   (operator entries kept, duplicates dropped);
/// - `[features] allow`: the role's toolset, intersected with the
///   operator layer's own allowlist when it carries one;
/// - `[fallback]`, and the fast role composed from `background_model`:
///   only the models `model_allowed` admits.
///
/// `model_allowed` is the acting user's model policy. The runtime reads
/// it from the store and passes it in (the port cannot); `true` for
/// every model when the user is unrestricted. It is consulted for the
/// models the layer names on the user's behalf — an operator's own
/// `[roles] fast` row is the operator's choice and stays.
///
/// Refused, naming the cause: a `[permissions] profile` activation (a
/// profile's rules are evaluated ahead of Ka Agent's approval rules, so
/// it could re-ask or deny what the approval profile allows;
/// `[[rules]]` is the supported form), an allow entry that can never
/// match a tool, and an allowlist intersection that leaves the role no
/// tool (the engine reads an empty `allow` as "everything").
pub fn compose_for_role(
    cfg: &DaemonConfig,
    role: Option<&ka_agent_core::RoleSpec>,
    user_layer: Option<&str>,
    resolve_secret: &dyn Fn(&str) -> Result<String, String>,
    model_allowed: &dyn Fn(&str) -> bool,
) -> Result<Option<String>, String> {
    let disable = disable_specs(cfg, role)?;
    let detected = match cfg.sandbox.as_str() {
        "fs" => true,
        "auto" => ka_sandbox::detect_tool().is_some(),
        _ => false,
    };
    // The pinned engine (ka `crates/ka-engine/src/voice.rs`, the
    // `Gate::Allow` arm of the bash gate: "grant-only ask") poses a
    // "sandbox grant for `…`" ask even when the permission gate returned
    // `Gate::Allow` — so `mode = "allow"` and `grants.full` must keep the
    // sandbox off entirely, or bash would still ask outside the
    // workspace.
    let sandbox_on = detected && cfg.mode != "allow" && !cfg.grants.full;
    // Allow-dir grants ride the sandbox allowlist and engine rules.
    let grant_dirs: Vec<&ka_agent_core::config::PathGrant> = cfg
        .grants
        .path
        .iter()
        .filter(|g| g.access == ka_agent_core::config::Access::Allow)
        .collect();
    let mut doc: toml::Table = match user_layer {
        Some(text) => text.parse().map_err(|e| format!("engine layer: {e}"))?,
        None => toml::Table::new(),
    };
    // A permission profile puts its rules ahead of `[[rules]]` — and so
    // ahead of the exact-tool allow rules the worker inserts for Ka
    // Agent's approval profile. Refused whatever else the layer carries.
    if doc
        .get("permissions")
        .and_then(|p| p.get("profile"))
        .is_some()
    {
        return Err(
            "engine layer: `[permissions] profile` is not supported — a profile's rules are \
             evaluated ahead of Ka Agent's approval rules, so they could re-ask or deny \
             what the approval profile allows; use [[rules]]"
                .to_string(),
        );
    }
    // Models the layer names on the user's behalf: the fallback chain,
    // and the fast role composed from `background_model` (recaps,
    // auto_review and the other engine chores are inert without one).
    let fallback: Vec<&String> = cfg.fallback.iter().filter(|m| model_allowed(m)).collect();
    // A role's layer also drops the models it may not use from the
    // operator's own chain, which is profile-wide otherwise.
    let operator_fallback_narrowed = role.is_some()
        && operator_fallback_models(&doc)?
            .iter()
            .any(|m| !model_allowed(m));
    let operator_fast = operator_fast_row(&doc);
    let fast = match cfg.background_model.as_deref() {
        Some(model) if !operator_fast && model_allowed(model) => Some(model),
        _ => None,
    };
    let allow = match role {
        Some(role) => allow_entries(role, cfg, &doc)?,
        None => Vec::new(),
    };
    let idle_minutes = (cfg.recap.idle_minutes > 0).then_some(cfg.recap.idle_minutes);
    let has_extras = cfg.search.is_some()
        || !fallback.is_empty()
        || operator_fallback_narrowed
        || fast.is_some()
        || !allow.is_empty()
        || idle_minutes.is_some()
        || !cfg.mcp.is_empty()
        || cfg.approvals.auto_review
        || cfg.effort.is_some()
        || sandbox_on
        || cfg.budgets.daily_usd.is_some()
        || role.is_some_and(|r| r.budget_daily_usd.is_some())
        || !disable.is_empty()
        || cfg
            .hooks
            .iter()
            .any(|h| ka_agent_core::config::ENGINE_HOOK_EVENTS.contains(&h.on.as_str()));
    if !has_extras {
        return Ok(user_layer.map(str::to_string));
    }

    // Feature toggles: subtractive and fail-closed in the engine. The
    // operator layer's entries survive; ours append (deduplicated).
    if !disable.is_empty() {
        let features = doc
            .entry("features")
            .or_insert_with(|| toml::Table::new().into());
        let features = features
            .as_table_mut()
            .ok_or_else(|| "engine layer: `features` must be a table".to_string())?;
        let list = features
            .entry("disable")
            .or_insert_with(|| toml::Value::Array(Vec::new()));
        let list = list
            .as_array_mut()
            .ok_or_else(|| "engine layer: `features.disable` must be an array".to_string())?;
        for spec in disable {
            let value = toml::Value::String(spec);
            if !list.contains(&value) {
                list.push(value);
            }
        }
    }

    // The role's toolset as the engine's positive allowlist: only the
    // listed hands are offered, and a stray call is rejected by name.
    // The per-turn `allowed_tools` carries the same entries and keeps
    // narrowing each turn; this row makes the posture the session's own.
    if !allow.is_empty() {
        let features = doc
            .entry("features")
            .or_insert_with(|| toml::Table::new().into());
        let features = features
            .as_table_mut()
            .ok_or_else(|| "engine layer: `features` must be a table".to_string())?;
        features.insert(
            "allow".into(),
            toml::Value::Array(allow.iter().map(|a| a.clone().into()).collect()),
        );
    }

    if let Some(search) = &cfg.search {
        let mut row = toml::Table::new();
        row.insert("provider".into(), search.provider.clone().into());
        row.insert("api_key_env".into(), search.key_env().into());
        if let Some(base) = &search.base_url {
            row.insert("base_url".into(), base.clone().into());
        }
        // `[[search]]` is a fallback chain, tried in order. Ka Agent's row
        // goes first (its choice wins); the operator's rows follow as
        // fallbacks, minus a row for the provider Ka Agent already named.
        let mut rows = match doc.remove("search") {
            None => Vec::new(),
            Some(toml::Value::Array(rows)) => rows,
            Some(_) => {
                return Err(
                    "engine layer: `search` must be an array of tables (`[[search]]`)".into(),
                );
            }
        };
        rows.retain(|r| {
            r.get("provider").and_then(toml::Value::as_str) != Some(search.provider.as_str())
        });
        rows.insert(0, row.into());
        doc.insert("search".into(), toml::Value::Array(rows));
    }

    if !fallback.is_empty() {
        let mut fb = toml::Table::new();
        fb.insert(
            "models".into(),
            toml::Value::Array(fallback.iter().map(|m| (*m).clone().into()).collect()),
        );
        doc.insert("fallback".into(), fb.into());
    } else if operator_fallback_narrowed {
        // The operator's chain stands in for an unset `fallback`: for a
        // role it keeps only the models the user may use.
        narrow_operator_fallback(&mut doc, model_allowed)?;
    }

    // The engine's chores (idle recaps, auto_review, prewalk) run on the
    // fast role; the background model is the operator's cheap one.
    if let Some(fast) = fast {
        let roles = doc
            .entry("roles")
            .or_insert_with(|| toml::Table::new().into());
        let roles = roles
            .as_table_mut()
            .ok_or_else(|| "engine layer: `roles` must be a table".to_string())?;
        roles.insert("fast".into(), fast.into());
    }

    if let Some(minutes) = idle_minutes {
        let context = doc
            .entry("context")
            .or_insert_with(|| toml::Table::new().into());
        let context = context
            .as_table_mut()
            .ok_or_else(|| "engine layer: `context` must be a table".to_string())?;
        let minutes = i64::try_from(minutes)
            .map(toml::Value::from)
            .map_err(|e| format!("engine layer: recap.idle_minutes: {e}"))?;
        context.insert("idle_minutes".into(), minutes);
    }

    if cfg.approvals.auto_review {
        let guards = doc
            .entry("guards")
            .or_insert_with(|| toml::Table::new().into());
        if let Some(g) = guards.as_table_mut() {
            g.insert("auto_review".into(), true.into());
        }
    }

    // Engine hooks: the engine runs them with its own semantics (`sh -c`
    // or an HTTP handler, `timeout_secs` per hook, exit 2 blocks, stdout
    // steers). Agent-event hooks are the runtime's (HookSurface), never
    // the engine's.
    {
        let engine_hooks: Vec<&ka_agent_core::config::HookSpec> = cfg
            .hooks
            .iter()
            .filter(|h| ka_agent_core::config::ENGINE_HOOK_EVENTS.contains(&h.on.as_str()))
            .collect();
        if !engine_hooks.is_empty() {
            let mut rows = match doc.remove("hooks") {
                Some(toml::Value::Array(a)) => a,
                Some(_) => return Err("engine layer: `hooks` must be an array of tables".into()),
                None => Vec::new(),
            };
            for h in engine_hooks {
                let mut row = toml::Table::new();
                row.insert("event".into(), h.on.clone().into());
                if let Some(tool) = &h.tool {
                    row.insert("tool".into(), tool.clone().into());
                }
                row.insert("command".into(), h.command.clone().into());
                if let Some(secs) = h.timeout_secs {
                    let secs = i64::try_from(secs)
                        .map(toml::Value::from)
                        .map_err(|e| format!("engine layer: hook {:?}: timeout_secs: {e}", h.on))?;
                    row.insert("timeout_secs".into(), secs);
                }
                // Cross-checked with the engine's own parser before any
                // spawn, like the disable specs: a hook event the engine
                // does not know fails here, named.
                toml::Value::Table(row.clone())
                    .try_into::<ka_engine::config::Hook>()
                    .map_err(|e| format!("engine layer: hook {:?}: {e}", h.on))?;
                rows.push(row.into());
            }
            doc.insert("hooks".into(), toml::Value::Array(rows));
        }
    }

    if let Some(effort) = &cfg.effort {
        doc.insert("effort".into(), effort.clone().into());
    }

    // The OS sandbox: `fs` restricts bash without the operator having to
    // know ka's own vocabulary; the engine fails closed when no backend
    // exists. Attachments stay writable (send_file, transcript exports).
    if sandbox_on && !doc.contains_key("sandbox") {
        let mut sandbox = toml::Table::new();
        sandbox.insert("mode".into(), "fs".into());
        // The attachments dir lives under the instance data root; the
        // engine only needs it for send_file staging and exports.
        let attachments = ka_agent_core::layout::default_root().join("attachments");
        sandbox.insert(
            "allow_write".into(),
            toml::Value::Array(vec![attachments.to_string_lossy().into_owned().into()]),
        );
        doc.insert("sandbox".into(), sandbox.into());
    }

    // Granted dirs join the sandbox allowlist whichever sandbox table
    // ended up in the layer (ours or the operator's): bash redirects
    // inside them must not compute an extra sandbox-grant ask.
    if sandbox_on && let Some(sandbox) = doc.get_mut("sandbox").and_then(toml::Value::as_table_mut)
    {
        let list = sandbox
            .entry("allow_write".to_string())
            .or_insert_with(|| toml::Value::Array(Vec::new()));
        if let toml::Value::Array(arr) = list {
            for g in &grant_dirs {
                let dir = toml::Value::String(g.dir.to_string_lossy().into_owned());
                if !arr.contains(&dir) {
                    arr.push(dir);
                }
            }
        }
    }

    // The engine's spend guard is the live budget: the continue-ask it
    // raises becomes an approval card the operator already knows. A role's
    // own USD cap replaces the profile's.
    let spend_cap = role
        .and_then(|r| r.budget_daily_usd)
        .or(cfg.budgets.daily_usd);
    if let Some(cap) = spend_cap {
        let guards = doc
            .entry("guards")
            .or_insert_with(|| toml::Table::new().into());
        if let Some(g) = guards.as_table_mut() {
            g.insert("spend_usd".into(), cap.into());
        }
    }

    if !cfg.mcp.is_empty() {
        let mut servers = match doc.remove("mcp") {
            Some(toml::Value::Array(a)) => a,
            Some(_) => return Err("engine layer: `mcp` must be an array of tables".into()),
            None => Vec::new(),
        };
        for m in &cfg.mcp {
            if !mcp_visible_to(m, role) {
                continue;
            }
            // Same-name dedupe (config wins): the engine persists the
            // servers it was opened with, so a warm engine home already
            // carries an injected `kagent` (or an old `[[mcp]]` def) —
            // appending again sends the provider duplicate tool names
            // ("tools: Tool names must be unique"). Replace, never stack.
            let row = mcp_row(m, resolve_secret)?;
            if let Some(existing) = servers
                .iter_mut()
                .find(|s| s.get("name").and_then(|n| n.as_str()) == Some(m.name.as_str()))
            {
                *existing = row.into();
            } else {
                servers.push(row.into());
            }
        }
        doc.insert("mcp".into(), toml::Value::Array(servers));
        // Several servers' full tool lists crowd the context: let the model
        // list a server's tools on demand instead (unless the operator's
        // layer already chose).
        if cfg.mcp.len() > 1 {
            let tools = doc
                .entry("tools")
                .or_insert_with(|| toml::Table::new().into());
            if let Some(tools) = tools.as_table_mut() {
                let mcp = tools
                    .entry("mcp")
                    .or_insert_with(|| toml::Table::new().into());
                if let Some(mcp) = mcp.as_table_mut() {
                    mcp.entry("discovery").or_insert_with(|| "lazy".into());
                }
            }
        }
    }

    toml::to_string(&doc)
        .map(Some)
        .map_err(|e| format!("engine layer: {e}"))
}

/// The model selectors in the operator layer's `[fallback] models`
/// (empty when the layer carries none).
fn operator_fallback_models(doc: &toml::Table) -> Result<Vec<&str>, String> {
    match doc.get("fallback").and_then(|f| f.get("models")) {
        None => Ok(Vec::new()),
        Some(toml::Value::Array(items)) => {
            Ok(items.iter().filter_map(toml::Value::as_str).collect())
        }
        Some(_) => Err("engine layer: `fallback.models` must be an array".into()),
    }
}

/// For a role, drop from the operator layer's `[fallback] models` the
/// models `model_allowed` refuses; a chain left empty removes the table.
/// Entries that are not strings stay for the engine's parser to report.
fn narrow_operator_fallback(
    doc: &mut toml::Table,
    model_allowed: &dyn Fn(&str) -> bool,
) -> Result<(), String> {
    let empty = {
        let Some(models) = doc.get_mut("fallback").and_then(|f| f.get_mut("models")) else {
            return Ok(());
        };
        let toml::Value::Array(items) = models else {
            return Err("engine layer: `fallback.models` must be an array".into());
        };
        items.retain(|v| v.as_str().is_none_or(model_allowed));
        items.is_empty()
    };
    if empty {
        doc.remove("fallback");
    }
    Ok(())
}

/// Whether a configured MCP server reaches these sessions: a server
/// without `roles` is for everyone; with `roles`, only for a named role
/// (the operator's own admin role is `None` here).
fn mcp_visible_to(m: &ka_agent_core::McpServer, role: Option<&ka_agent_core::RoleSpec>) -> bool {
    match (&m.roles, role.map(|r| r.name.as_str())) {
        (None, _) => true,
        (Some(roles), Some(name)) => roles.iter().any(|r| r == name),
        (Some(roles), None) => roles.iter().any(|r| r == ka_agent_core::ADMIN_ROLE),
    }
}

/// The MCP servers the composed layer carries for this role: the
/// operator layer's own `[[mcp]]` rows (never role-scoped) plus the
/// configured ones the role may see.
fn layer_mcp_servers(
    cfg: &DaemonConfig,
    role: Option<&ka_agent_core::RoleSpec>,
    doc: &toml::Table,
) -> Vec<String> {
    let operator = doc
        .get("mcp")
        .and_then(toml::Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|row| row.get("name"))
        .filter_map(toml::Value::as_str)
        .map(str::to_string);
    let configured = cfg
        .mcp
        .iter()
        .filter(|m| mcp_visible_to(m, role))
        .map(|m| m.name.clone());
    operator.chain(configured).collect()
}

/// Whether an operator `features.allow` entry admits a role entry: by
/// name; a bare `mcp` for every server entry; an exact `kagent__*` tool
/// for the bridge's collapsed `mcp:kagent`.
fn operator_admits(operator: &str, entry: &str) -> bool {
    operator == entry
        || (operator == "mcp" && entry.starts_with("mcp:"))
        || (entry == "mcp:kagent" && operator.starts_with("kagent__"))
}

/// Feature-toggle words that are not hand names, so they match nothing
/// in an allowlist. (`cron`, `monitor` and `generate_image` are both a
/// toggle and a hand, and stay valid entries.)
const TOGGLE_ONLY: &[&str] = &["agents", "skills", "hooks", "web", "lsp", "debug"];

/// The engine's `[features] allow` for a role's toolset (empty for a
/// role without `allow`): the role's entries in the engine's allowlist
/// grammar — hand names as they are (the names the per-turn
/// `allowed_tools` carries), every `kagent__*` tool collapsed into the
/// one `mcp:kagent` entry, the engine's own way to admit a server's
/// hands. Intersected with the operator layer's `features.allow` when it
/// carries one: the engine keeps a single list, and a union would widen
/// the operator's allowlist.
///
/// Fail closed, naming the role and the entry, for an entry that can
/// never match a tool: a feature toggle (`tool:bash`, `web`, …) belongs
/// in a disable list, and `mcp:<server>` must name a server the layer
/// has (`kagent` is the bridge the worker always injects). A misspelled
/// hand name is not caught: the engine exposes no roster of its
/// config-gated hands to compare against, and refusing on a guess would
/// turn a valid entry into a dead role.
fn allow_entries(
    role: &ka_agent_core::RoleSpec,
    cfg: &DaemonConfig,
    doc: &toml::Table,
) -> Result<Vec<String>, String> {
    if role.allow.is_empty() {
        return Ok(Vec::new());
    }
    let mut servers = layer_mcp_servers(cfg, Some(role), doc);
    servers.push("kagent".into());
    let mut entries: Vec<String> = Vec::new();
    for entry in &role.allow {
        let mapped = match entry.strip_prefix("mcp:") {
            Some(server) if !servers.iter().any(|s| s == server) => {
                return Err(format!(
                    "role {:?}: allow entry {entry:?} names no MCP server this role can see \
                     (in the layer: {})",
                    role.name,
                    servers.join(", ")
                ));
            }
            Some(_) => entry.clone(),
            None if TOGGLE_ONLY.contains(&entry.as_str())
                || ["tool:", "skill:", "agent:"]
                    .iter()
                    .any(|p| entry.starts_with(p)) =>
            {
                return Err(format!(
                    "role {:?}: allow entry {entry:?} is a feature toggle, not a tool — disable \
                     specs go in the role's `disable` (or the profile's `tools.disable`)",
                    role.name
                ));
            }
            None if entry.starts_with("kagent__") => "mcp:kagent".to_string(),
            None => entry.clone(),
        };
        if !entries.contains(&mapped) {
            entries.push(mapped);
        }
    }
    let Some(operator) = doc.get("features").and_then(|f| f.get("allow")) else {
        return Ok(entries);
    };
    let toml::Value::Array(items) = operator else {
        return Err("engine layer: `features.allow` must be an array of tool names".into());
    };
    let operator: Vec<&str> = items
        .iter()
        .map(|item| {
            item.as_str().ok_or_else(|| {
                "engine layer: `features.allow` must be an array of tool names".to_string()
            })
        })
        .collect::<Result<_, _>>()?;
    entries.retain(|entry| operator.iter().any(|o| operator_admits(o, entry)));
    if entries.is_empty() {
        return Err(format!(
            "role {:?}: its toolset shares nothing with the operator layer's `features.allow` \
             ({}) — the session would see no tool",
            role.name,
            operator.join(", ")
        ));
    }
    Ok(entries)
}

fn resolve_value(
    server: &str,
    v: &str,
    resolve_secret: &dyn Fn(&str) -> Result<String, String>,
) -> Result<String, String> {
    match v.strip_prefix(SECRET_PREFIX) {
        Some(name) => resolve_secret(name)
            .map_err(|e| format!("mcp {server:?}: {e} — `ka-agent secret set {name}`")),
        None => Ok(v.to_string()),
    }
}

fn mcp_row(
    m: &ka_agent_core::McpServer,
    resolve_secret: &dyn Fn(&str) -> Result<String, String>,
) -> Result<toml::Table, String> {
    let mut row = toml::Table::new();
    row.insert("name".into(), m.name.clone().into());
    if let Some(cmd) = &m.command {
        row.insert("command".into(), cmd.clone().into());
        row.insert(
            "args".into(),
            toml::Value::Array(m.args.iter().cloned().map(Into::into).collect()),
        );
        let mut env = toml::Table::new();
        for (k, v) in &m.env {
            env.insert(k.clone(), resolve_value(&m.name, v, resolve_secret)?.into());
        }
        row.insert("env".into(), env.into());
    }
    if let Some(url) = &m.url {
        row.insert("url".into(), url.clone().into());
        let mut headers = Vec::new();
        for (k, v) in &m.headers {
            headers.push(toml::Value::Array(vec![
                k.clone().into(),
                resolve_value(&m.name, v, resolve_secret)?.into(),
            ]));
        }
        row.insert("headers".into(), toml::Value::Array(headers));
    }
    Ok(row)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    fn secrets(name: &str) -> Result<String, String> {
        match name {
            "gh.token" => Ok("ghp_x".into()),
            other => Err(format!("secret {other:?} not set")),
        }
    }

    /// A user whose group policy allows every model.
    fn every_model(_: &str) -> bool {
        true
    }

    fn parse(text: &str) -> ka_engine::Config {
        let mut cfg = ka_engine::Config::default();
        cfg.overlay(ka_engine::Config::parse_layer(text, "test").unwrap());
        cfg
    }

    #[test]
    fn nothing_configured_passes_the_user_layer_through() {
        // `sandbox = "auto"` composes an fs layer wherever a backend exists
        // (2.5); the pass-through contract is about every OTHER knob.
        let cfg = DaemonConfig {
            sandbox: "off".into(),
            ..DaemonConfig::default()
        };
        assert_eq!(compose(&cfg, None, &secrets).unwrap(), None);
        assert_eq!(
            compose(&cfg, Some("mode = \"plan\"\n"), &secrets)
                .unwrap()
                .as_deref(),
            Some("mode = \"plan\"\n")
        );
    }

    #[test]
    fn mcp_servers_are_gated_by_role() {
        let cfg: DaemonConfig = toml::from_str(
            r#"
[[mcp]]
name = "shared"
command = "shared-mcp"
[[mcp]]
name = "devonly"
command = "dev-mcp"
roles = ["dev"]
[[mcp]]
name = "adminonly"
command = "admin-mcp"
roles = ["admin"]
[[role]]
name = "dev"
mode = "readonly"
allow = ["read"]
"#,
        )
        .unwrap();
        let dev = cfg.roles.iter().find(|r| r.name == "dev").unwrap();
        // The dev role sees the shared and dev servers, never the admin one.
        let text = compose_for_role(&cfg, Some(dev), None, &secrets, &every_model)
            .unwrap()
            .expect("a layer");
        assert!(text.contains("shared-mcp"), "{text}");
        assert!(text.contains("dev-mcp"), "{text}");
        assert!(!text.contains("admin-mcp"), "{text}");
        // Admin (no role spec) sees the shared and admin servers.
        let text = compose_for_role(&cfg, None, None, &secrets, &every_model)
            .unwrap()
            .expect("a layer");
        assert!(text.contains("shared-mcp"), "{text}");
        assert!(text.contains("admin-mcp"), "{text}");
        assert!(!text.contains("dev-mcp"), "{text}");
        // A validation error names a role that is not configured.
        let bad: Result<DaemonConfig, _> = toml::from_str(
            r#"
[[mcp]]
name = "x"
command = "x"
roles = ["ghost"]
"#,
        );
        let bad = bad.unwrap();
        assert!(bad.validate().is_err());
    }

    #[test]
    fn search_fallback_and_mcp_reach_the_engine_config() {
        let cfg: DaemonConfig = toml::from_str(
            r#"
fallback = ["openai/gpt-5.1"]
[search]
provider = "tavily"
[[mcp]]
name = "github"
command = "gh-mcp"
args = ["--stdio"]
env = { GITHUB_TOKEN = "secret:gh.token", PLAIN = "v" }
[[mcp]]
name = "home"
url = "http://127.0.0.1:8123/mcp"
headers = { Authorization = "secret:gh.token" }
"#,
        )
        .unwrap();
        let text = compose(
            &cfg,
            Some("[[mcp]]\nname = \"mine\"\ncommand = \"x\"\n"),
            &secrets,
        )
        .unwrap()
        .unwrap();
        let engine = parse(&text);
        let search = engine.effective_search();
        assert_eq!(search.len(), 1, "one row without an operator chain");
        assert_eq!(search[0].provider, "tavily");
        assert_eq!(search[0].api_key_env.as_deref(), Some("TAVILY_API_KEY"));
        assert_eq!(engine.fallback.models, vec!["openai/gpt-5.1".to_string()]);
        let names: Vec<&str> = engine.mcp.iter().map(|m| m.name.as_str()).collect();
        assert_eq!(
            names,
            vec!["mine", "github", "home"],
            "operator layer kept, ours appended"
        );
        let gh = &engine.mcp[1];
        assert_eq!(
            gh.env.get("GITHUB_TOKEN").map(String::as_str),
            Some("ghp_x")
        );
        assert_eq!(gh.env.get("PLAIN").map(String::as_str), Some("v"));
        assert_eq!(
            engine.mcp[2].headers,
            vec![("Authorization".to_string(), "ghp_x".to_string())]
        );
        assert_eq!(
            engine.tools.mcp.discovery,
            Some(ka_engine::config::McpDiscovery::Lazy)
        );
        // The secret is resolved into the in-memory layer only.
        assert!(!text.contains("secret:"));
    }

    #[test]
    fn auto_review_turns_on_the_engine_guard() {
        let cfg: DaemonConfig = toml::from_str("[approvals]\nauto_review = true\n").unwrap();
        let text = compose(&cfg, None, &secrets).unwrap().unwrap();
        assert_eq!(parse(&text).guards.auto_review, Some(true));
        let off = DaemonConfig {
            sandbox: "off".into(),
            ..DaemonConfig::default()
        };
        assert_eq!(compose(&off, None, &secrets).unwrap(), None);
    }

    #[test]
    fn a_missing_secret_names_the_fix() {
        let cfg: DaemonConfig = toml::from_str(
            "[[mcp]]\nname = \"x\"\ncommand = \"c\"\nenv = { T = \"secret:nope\" }\n",
        )
        .unwrap();
        let err = compose(&cfg, None, &secrets).unwrap_err();
        assert!(err.contains("ka-agent secret set nope"), "{err}");
    }

    #[test]
    fn baseline_and_role_disable_specs_merge_into_the_layer() {
        let mut cfg = DaemonConfig {
            sandbox: "off".into(),
            ..DaemonConfig::default()
        };
        cfg.tools.disable = vec!["tool:bash".into(), "mcp:github".into()];
        let mut guest = ka_agent_core::RoleSpec {
            name: "guest".into(),
            allow: Vec::new(),
            ..Default::default()
        };
        guest.disable = vec!["mcp:github".into(), "tool:write".into()];

        // Admin: the profile baseline only.
        let admin = compose_for_role(&cfg, None, None, &secrets, &every_model)
            .unwrap()
            .unwrap();
        let parsed: toml::Value = toml::from_str(&admin).unwrap();
        let specs: Vec<&str> = parsed["features"]["disable"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|v| v.as_str())
            .collect();
        assert_eq!(specs, vec!["tool:bash", "mcp:github"]);

        // Role: baseline + role, deduplicated in order.
        let layer = compose_for_role(&cfg, Some(&guest), None, &secrets, &every_model)
            .unwrap()
            .unwrap();
        let parsed: toml::Value = toml::from_str(&layer).unwrap();
        let specs: Vec<&str> = parsed["features"]["disable"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|v| v.as_str())
            .collect();
        assert_eq!(specs, vec!["tool:bash", "mcp:github", "tool:write"]);
        // The engine parses the composed layer without complaint.
        let engine = parse(&layer);
        assert_eq!(engine.features.disable.len(), 3);
    }

    #[test]
    fn operator_feature_entries_survive_and_ours_append() {
        let cfg = DaemonConfig {
            sandbox: "off".into(),
            ..DaemonConfig::default()
        };
        let mut cfg = cfg;
        cfg.tools.disable = vec!["tool:bash".into(), "web".into()];
        let user = "[features]\ndisable = [\"debug\", \"tool:bash\"]\n";
        let text = compose_for_role(&cfg, None, Some(user), &secrets, &every_model)
            .unwrap()
            .unwrap();
        let parsed: toml::Value = toml::from_str(&text).unwrap();
        let specs: Vec<&str> = parsed["features"]["disable"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|v| v.as_str())
            .collect();
        assert_eq!(specs, vec!["debug", "tool:bash", "web"]);
    }

    #[test]
    fn an_invalid_spec_is_refused_before_any_spawn() {
        let mut cfg = DaemonConfig {
            sandbox: "off".into(),
            ..DaemonConfig::default()
        };
        cfg.tools.disable = vec!["Tool:Bash".into()];
        let err = compose_for_role(&cfg, None, None, &secrets, &every_model).unwrap_err();
        assert!(err.contains("Tool:Bash"), "{err}");
        // A hand-built role with a bad spec is refused too (the config
        // loader would have caught it; compose is the last gate).
        cfg.tools.disable.clear();
        let role = ka_agent_core::RoleSpec {
            name: "guest".into(),
            disable: vec!["nope".into()],
            ..Default::default()
        };
        let err = compose_for_role(&cfg, Some(&role), None, &secrets, &every_model).unwrap_err();
        assert!(err.contains("nope"), "{err}");
    }

    /// Core duplicates the engine's feature grammar; every spec core
    /// accepts MUST parse with the engine's own parser (the reverse is
    /// not required: core is deliberately stricter about names).
    #[test]
    fn core_and_engine_feature_grammars_agree_on_valid_specs() {
        let fixture = [
            "agents",
            "skills",
            "mcp",
            "hooks",
            "web",
            "lsp",
            "debug",
            "cron",
            "monitor",
            "generate_image",
            "mcp:github",
            "mcp:my_server-2",
            "skill:release-notes",
            "tool:bash",
            "tool:kagent__notes_search",
            "agent:reviewer",
        ];
        for spec in fixture {
            assert!(ka_agent_core::valid_feature_spec(spec), "core: {spec}");
            assert!(
                spec.parse::<ka_protocol::FeatureSpec>().is_ok(),
                "engine: {spec}"
            );
        }
        // The engine is more permissive (`tool:bash:x` parses); core
        // rejects it, and compose never lets it reach the engine.
        assert!(!ka_agent_core::valid_feature_spec("tool:bash:x"));
        assert!("tool:bash:x".parse::<ka_protocol::FeatureSpec>().is_ok());
    }
}

#[cfg(test)]
mod step25_tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    fn no_secret(_name: &str) -> Result<String, String> {
        Err("no secrets".into())
    }

    #[test]
    fn effort_sandbox_and_spend_guard_compose() {
        let cfg = ka_agent_core::DaemonConfig {
            effort: Some("high".into()),
            sandbox: "fs".into(),
            budgets: ka_agent_core::config::Budgets {
                daily_usd: Some(2.5),
                ..Default::default()
            },
            ..Default::default()
        };
        let layer = compose(&cfg, None, &no_secret).unwrap().expect("a layer");
        let parsed: toml::Value = toml::from_str(&layer).unwrap();
        assert_eq!(parsed["effort"].as_str(), Some("high"));
        assert_eq!(parsed["sandbox"]["mode"].as_str(), Some("fs"));
        assert!(
            parsed["sandbox"]["allow_write"][0]
                .as_str()
                .unwrap()
                .ends_with("attachments")
        );
        assert_eq!(parsed["guards"]["spend_usd"].as_float(), Some(2.5));
    }

    #[test]
    fn sandbox_off_composes_nothing() {
        let cfg = ka_agent_core::DaemonConfig {
            sandbox: "off".into(),
            ..Default::default()
        };
        assert!(compose(&cfg, None, &no_secret).unwrap().is_none());
    }

    #[test]
    fn allow_and_full_modes_keep_the_sandbox_off() {
        // The pinned engine asks for sandbox grants even under Gate::Allow
        // (voice.rs ~2720), so allow/full must not compose [sandbox] at all.
        let mut cfg = ka_agent_core::DaemonConfig {
            sandbox: "fs".into(),
            mode: "allow".into(),
            ..Default::default()
        };
        let layer = compose(&cfg, None, &no_secret).unwrap();
        assert!(layer.is_none(), "no extras -> pass-through: {layer:?}");
        // With an extra the layer exists but carries no sandbox table.
        cfg.effort = Some("low".into());
        let parsed: toml::Value =
            toml::from_str(&compose(&cfg, None, &no_secret).unwrap().unwrap()).unwrap();
        assert!(parsed.get("sandbox").is_none());

        let mut full = ka_agent_core::DaemonConfig {
            sandbox: "fs".into(),
            effort: Some("low".into()),
            ..Default::default()
        };
        full.grants.full = true;
        let parsed: toml::Value =
            toml::from_str(&compose(&full, None, &no_secret).unwrap().unwrap()).unwrap();
        assert!(parsed.get("sandbox").is_none());
    }

    #[test]
    fn allow_dirs_join_the_sandbox_only() {
        let cfg: ka_agent_core::DaemonConfig = toml::from_str(
            "sandbox = \"fs\"\n\
             [[grants.path]]\ndir = \"/srv/ok\"\naccess = \"always\"\n\
             [[grants.path]]\ndir = \"/srv/ok2\"\naccess = \"allow\"\n\
             [[grants.path]]\ndir = \"/srv/nope\"\naccess = \"deny\"\n",
        )
        .unwrap();
        let text = compose(&cfg, None, &no_secret).unwrap().unwrap();
        let parsed: toml::Value = toml::from_str(&text).unwrap();
        let writes: Vec<&str> = parsed["sandbox"]["allow_write"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|v| v.as_str())
            .collect();
        assert!(writes.contains(&"/srv/ok"), "{writes:?}");
        assert!(writes.contains(&"/srv/ok2"), "{writes:?}");
        assert!(
            !writes.contains(&"/srv/nope"),
            "deny dirs never widen the sandbox: {writes:?}"
        );
        // No engine rules are composed for grant dirs: the `__guard`
        // hook pre-approves writes inside them (write/edit), so N
        // granted dirs behave identically.
        let engine = ka_engine::Config::parse_layer(&text, "t").unwrap();
        assert!(
            engine
                .rules
                .iter()
                .all(|r| !r.pattern.as_deref().is_some_and(|p| p.contains("/srv/"))),
            "{:?}",
            engine.rules
        );
    }

    #[test]
    fn user_layer_rules_survive_grant_composition_untouched() {
        // Sandbox off + a grant dir composes nothing at all (the guard
        // handles the grant), so the operator's rules pass through
        // verbatim.
        let cfg: ka_agent_core::DaemonConfig = toml::from_str(
            "sandbox = \"off\"\n[[grants.path]]\ndir = \"/srv/ok\"\naccess = \"allow\"\n",
        )
        .unwrap();
        let user = "[[rules]]\ntool = \"write\"\npattern = \"secret/**\"\nverdict = \"deny\"\n";
        assert_eq!(
            compose(&cfg, Some(user), &no_secret).unwrap().as_deref(),
            Some(user)
        );
    }
}

/// The per-role posture and the v0.12 engine knobs: `[features] allow`,
/// the ACL-filtered fallback chain, `[roles] fast`, the search chain,
/// hooks v2, idle recaps and the refused permission profile. Each test
/// reads the composed layer back with the engine's own parser.
#[cfg(test)]
mod posture_tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;
    use ka_agent_core::config::HookSpec;

    fn no_secret(_: &str) -> Result<String, String> {
        Err("no secrets".into())
    }

    fn parse(text: &str) -> ka_engine::Config {
        let mut cfg = ka_engine::Config::default();
        cfg.overlay(ka_engine::Config::parse_layer(text, "test").unwrap());
        cfg
    }

    /// A config with the sandbox off: nothing composes unless a test
    /// turns it on.
    fn off() -> DaemonConfig {
        DaemonConfig {
            sandbox: "off".into(),
            ..DaemonConfig::default()
        }
    }

    fn role(name: &str, allow: &[&str]) -> ka_agent_core::RoleSpec {
        ka_agent_core::RoleSpec {
            name: name.into(),
            allow: allow.iter().map(|a| a.to_string()).collect(),
            ..Default::default()
        }
    }

    fn strings(doc: &toml::Value, path: &[&str]) -> Vec<String> {
        let mut cur = doc;
        for key in path {
            cur = &cur[*key];
        }
        cur.as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap().to_string())
            .collect()
    }

    fn layer(
        cfg: &DaemonConfig,
        role: Option<&ka_agent_core::RoleSpec>,
        user: Option<&str>,
    ) -> String {
        compose_for_role(cfg, role, user, &no_secret, &|_| true)
            .unwrap()
            .unwrap()
    }

    #[test]
    fn role_toolset_becomes_the_engine_allowlist() {
        let cfg = off();
        let guest = role(
            "guest",
            &[
                "read",
                "grep",
                "kagent__notes_search",
                "kagent__memory_propose",
                "web_search",
            ],
        );
        let text = layer(&cfg, Some(&guest), None);
        let parsed: toml::Value = toml::from_str(&text).unwrap();
        assert_eq!(
            strings(&parsed, &["features", "allow"]),
            ["read", "grep", "mcp:kagent", "web_search"],
            "hand names as they are, the bridge once: {text}"
        );
        // The engine reads it as the positive allowlist, and it admits
        // what the per-turn list admits: the listed hands and the
        // bridge's, nothing else.
        let engine = parse(&text);
        let toggles = ka_engine::features::FeatureToggles::slot_with(
            Vec::new(),
            engine.features.allow.clone(),
        );
        let guard = toggles.read();
        for tool in [
            "read",
            "grep",
            "web_search",
            "kagent__notes_search",
            "kagent__scout",
        ] {
            assert!(guard.tool_visible(tool), "{tool}");
        }
        for tool in ["bash", "edit", "write", "delegate", "todo", "tool_search"] {
            assert!(!guard.tool_visible(tool), "{tool}");
        }
        drop(guard);

        // `disable` wins over `allow`: the profile baseline still hides.
        let mut cfg = cfg;
        cfg.tools.disable = vec!["tool:grep".into()];
        let engine = parse(&layer(&cfg, Some(&guest), None));
        let toggles = ka_engine::features::FeatureToggles::slot_with(
            vec!["tool:grep".parse().unwrap()],
            engine.features.allow.clone(),
        );
        let guard = toggles.read();
        assert!(!guard.tool_visible("grep"), "a disabled hand stays hidden");
        assert!(guard.tool_visible("read"));
    }

    #[test]
    fn a_role_without_a_toolset_and_the_admin_get_no_allowlist() {
        let cfg = off();
        let writer = ka_agent_core::RoleSpec {
            name: "writer".into(),
            disable: vec!["tool:bash".into()],
            ..Default::default()
        };
        let text = layer(&cfg, Some(&writer), None);
        let parsed: toml::Value = toml::from_str(&text).unwrap();
        assert!(
            parsed["features"].get("allow").is_none(),
            "no allow row without an allow list: {text}"
        );
        // The admin role is never narrowed.
        assert_eq!(
            compose_for_role(&cfg, None, None, &no_secret, &|_| true).unwrap(),
            None
        );
    }

    #[test]
    fn an_operator_allowlist_is_intersected_never_widened() {
        let cfg = off();
        let guest = role("guest", &["read", "grep", "kagent__notes_search"]);
        let user = "[features]\nallow = [\"read\", \"kagent__notes_search\", \"bash\"]\n";
        let text = layer(&cfg, Some(&guest), Some(user));
        let parsed: toml::Value = toml::from_str(&text).unwrap();
        assert_eq!(
            strings(&parsed, &["features", "allow"]),
            ["read", "mcp:kagent"],
            "{text}"
        );
        // Nothing in common is refused: the engine reads an empty
        // allowlist as "everything".
        let user = "[features]\nallow = [\"bash\"]\n";
        let err =
            compose_for_role(&cfg, Some(&guest), Some(user), &no_secret, &|_| true).unwrap_err();
        assert!(
            err.contains("role \"guest\"") && err.contains("(bash)"),
            "{err}"
        );
        // The operator's own list is the operator's business: untouched.
        assert_eq!(
            compose_for_role(&cfg, None, Some(user), &no_secret, &|_| true)
                .unwrap()
                .as_deref(),
            Some(user)
        );
    }

    #[test]
    fn allow_entries_that_can_never_match_a_tool_are_refused() {
        let cfg = off();
        // A disable spec under `allow` — a tier word or a prefixed form.
        for spec in ["tool:bash", "web", "agents"] {
            let guest = role("guest", &["read", spec]);
            let err =
                compose_for_role(&cfg, Some(&guest), None, &no_secret, &|_| true).unwrap_err();
            assert!(
                err.contains("role \"guest\"")
                    && err.contains(&format!("allow entry {spec:?}"))
                    && err.contains("feature toggle"),
                "{spec}: {err}"
            );
        }

        // `cron` is a hand as much as a toggle: a valid entry, moot only
        // because the worker's overlay disables the hand.
        let guest = role("guest", &["read", "cron"]);
        let text = layer(&cfg, Some(&guest), None);
        let parsed: toml::Value = toml::from_str(&text).unwrap();
        assert_eq!(strings(&parsed, &["features", "allow"]), ["read", "cron"]);
        // `mcp:<server>` must name a server the layer carries: the
        // bridge always, a configured one the role may see.
        let cfg: DaemonConfig = toml::from_str(
            "sandbox = \"off\"\n\
             [[mcp]]\nname = \"shared\"\ncommand = \"shared-mcp\"\n\
             [[mcp]]\nname = \"devonly\"\ncommand = \"dev-mcp\"\nroles = [\"dev\"]\n",
        )
        .unwrap();
        for server in ["mcp:ghost", "mcp:github", "mcp:"] {
            let outsider = role("guest", &["read", server]);
            let err =
                compose_for_role(&cfg, Some(&outsider), None, &no_secret, &|_| true).unwrap_err();
            assert!(
                err.contains("role \"guest\"")
                    && err.contains(&format!("allow entry {server:?}"))
                    && err.contains("no MCP server"),
                "{server}: {err}"
            );
        }
        let outsider = role("guest", &["mcp:devonly"]);
        assert!(compose_for_role(&cfg, Some(&outsider), None, &no_secret, &|_| true).is_err());
        let insider = role("dev", &["mcp:devonly", "mcp:kagent"]);
        let text = layer(&cfg, Some(&insider), None);
        let parsed: toml::Value = toml::from_str(&text).unwrap();
        assert_eq!(
            strings(&parsed, &["features", "allow"]),
            ["mcp:devonly", "mcp:kagent"]
        );
        // An operator-layer server counts too, and bare `mcp` is the
        // engine's "every server" entry.
        let user = "[[mcp]]\nname = \"mine\"\ncommand = \"x\"\n";
        let ops = role("ops", &["mcp:mine", "mcp"]);
        let text = layer(&cfg, Some(&ops), Some(user));
        let parsed: toml::Value = toml::from_str(&text).unwrap();
        assert_eq!(
            strings(&parsed, &["features", "allow"]),
            ["mcp:mine", "mcp"]
        );
    }

    #[test]
    fn fallback_keeps_only_the_models_the_user_may_use() {
        let mut cfg = off();
        cfg.fallback = vec!["openai/gpt-5.1".into(), "anthropic/claude-sonnet-5".into()];
        let anthropic_only = |m: &str| m.starts_with("anthropic/");
        let guest = role("guest", &["read"]);
        let text = compose_for_role(&cfg, Some(&guest), None, &no_secret, &anthropic_only)
            .unwrap()
            .unwrap();
        let parsed: toml::Value = toml::from_str(&text).unwrap();
        assert_eq!(
            strings(&parsed, &["fallback", "models"]),
            ["anthropic/claude-sonnet-5"]
        );
        // Nothing left: no chain at all.
        let text = compose_for_role(&cfg, Some(&guest), None, &no_secret, &|_| false)
            .unwrap()
            .unwrap();
        assert!(!text.contains("fallback"), "{text}");
        // The operator's own chain — profile-wide otherwise — is
        // narrowed the same way for a role ...
        let user = "[fallback]\nmodels = [\"openai/gpt-5.1\", \"anthropic/claude-sonnet-5\"]\n";
        cfg.fallback.clear();
        let text = compose_for_role(&cfg, Some(&guest), Some(user), &no_secret, &anthropic_only)
            .unwrap()
            .unwrap();
        let parsed: toml::Value = toml::from_str(&text).unwrap();
        assert_eq!(
            strings(&parsed, &["fallback", "models"]),
            ["anthropic/claude-sonnet-5"]
        );
        // ... down to nothing ...
        let text = compose_for_role(&cfg, Some(&guest), Some(user), &no_secret, &|_| false)
            .unwrap()
            .unwrap();
        assert!(!text.contains("fallback"), "{text}");
        // ... and left alone for the operator.
        assert_eq!(
            compose_for_role(&cfg, None, Some(user), &no_secret, &|_| false)
                .unwrap()
                .as_deref(),
            Some(user)
        );
    }

    #[test]
    fn the_fast_role_is_the_background_model() {
        let mut cfg = off();
        cfg.background_model = Some("openai/gpt-5.1".into());
        assert_eq!(
            parse(&layer(&cfg, None, None)).roles.fast.as_deref(),
            Some("openai/gpt-5.1")
        );
        // An operator row is the operator's choice: kept as written.
        let user = "[roles]\nfast = \"anthropic/claude-sonnet-5\"\n";
        assert_eq!(
            compose_for_role(&cfg, None, Some(user), &no_secret, &|_| true)
                .unwrap()
                .as_deref(),
            Some(user),
            "nothing left to add: pass-through"
        );
        // A user whose policy refuses the background model gets no fast
        // role rather than one on a model they may not use.
        let guest = role("guest", &["read"]);
        let text = compose_for_role(&cfg, Some(&guest), None, &no_secret, &|m| {
            m.starts_with("anthropic/")
        })
        .unwrap()
        .unwrap();
        assert!(parse(&text).roles.fast.is_none(), "{text}");
        // Unset: nothing is composed for it.
        cfg.background_model = None;
        assert_eq!(
            compose_for_role(&cfg, None, None, &no_secret, &|_| true).unwrap(),
            None
        );
    }

    /// `doctor` reads `has_fast_role` to say whether the fast-role chores
    /// can run: it must see both sources the composition draws on.
    #[test]
    fn the_operator_layer_carries_a_fast_role_from_either_source() {
        let mut cfg = off();
        assert!(!has_fast_role(&cfg, None), "nothing configured");
        assert!(!has_fast_role(&cfg, Some("[context]\nidle_minutes = 20\n")));
        assert!(!has_fast_role(&cfg, Some("not [ toml")));
        cfg.background_model = Some("openai/gpt-5.1".into());
        assert!(has_fast_role(&cfg, None));
        let user = "[roles]\nfast = \"anthropic/claude-sonnet-5\"\n";
        cfg.background_model = None;
        assert!(has_fast_role(&cfg, Some(user)));
        // What doctor reads is what the engine gets.
        let text = compose(&cfg, Some(user), &no_secret)
            .unwrap()
            .unwrap_or_else(|| user.to_string());
        assert!(parse(&text).roles.fast.is_some(), "{text}");
    }

    #[test]
    fn the_agents_search_row_leads_the_operators_chain() {
        let cfg: DaemonConfig =
            toml::from_str("sandbox = \"off\"\n[search]\nprovider = \"tavily\"\n").unwrap();
        let user = "[[search]]\nprovider = \"brave\"\n\
                    [[search]]\nprovider = \"tavily\"\napi_key_env = \"OTHER_KEY\"\n\
                    [[search]]\nprovider = \"duckduckgo\"\n";
        let text = compose(&cfg, Some(user), &no_secret).unwrap().unwrap();
        let chain = parse(&text).effective_search();
        let providers: Vec<&str> = chain.iter().map(|p| p.provider.as_str()).collect();
        assert_eq!(providers, ["tavily", "brave", "duckduckgo"], "{text}");
        assert_eq!(chain[0].api_key_env.as_deref(), Some("TAVILY_API_KEY"));
    }

    #[test]
    fn a_permission_profile_is_refused() {
        let cfg = off();
        let user = "[[permissions.profiles.locked.rules]]\ntool = \"bash\"\nverdict = \"deny\"\n\
                    [permissions]\nprofile = \"locked\"\n";
        let err = compose(&cfg, Some(user), &no_secret).unwrap_err();
        assert!(
            err.contains("`[permissions] profile` is not supported") && err.contains("[[rules]]"),
            "{err}"
        );
        // With nothing else to compose, for a role too.
        let guest = role("guest", &["read"]);
        assert!(compose_for_role(&cfg, Some(&guest), Some(user), &no_secret, &|_| true).is_err());
        // A profile that is only defined activates nothing: the engine's business.
        let defined =
            "[[permissions.profiles.locked.rules]]\ntool = \"bash\"\nverdict = \"deny\"\n";
        assert_eq!(
            compose(&cfg, Some(defined), &no_secret).unwrap().as_deref(),
            Some(defined)
        );
    }

    /// Core lists the engine's hook events; every name it lists must be
    /// one the engine's own parser accepts, and the per-hook timeout
    /// must reach the engine (hooks v2).
    #[test]
    fn every_engine_hook_event_composes_with_its_timeout() {
        for on in ka_agent_core::config::ENGINE_HOOK_EVENTS {
            let mut cfg = off();
            let tool = (*on == "pre_tool_use").then(|| "bash".to_string());
            cfg.hooks.push(HookSpec {
                on: (*on).to_string(),
                command: "true".into(),
                tool: tool.clone(),
                timeout_secs: Some(30),
            });
            let text = layer(&cfg, None, None);
            let engine = parse(&text);
            let hook = engine.hooks.last().unwrap();
            assert_eq!(hook.event.wire_name(), *on, "{text}");
            assert_eq!(hook.tool, tool, "a tool-scoped row keeps `tool`");
            assert_eq!(hook.timeout_secs, Some(30));
            assert_eq!(hook.effective_timeout_secs(), 30);
        }
        // Unset: the engine's 10 s default.
        let mut cfg = off();
        cfg.hooks.push(HookSpec {
            on: "permission_request".into(),
            command: "true".into(),
            tool: None,
            timeout_secs: None,
        });
        assert_eq!(
            parse(&layer(&cfg, None, None)).hooks[0].effective_timeout_secs(),
            ka_engine::config::DEFAULT_HOOK_TIMEOUT_SECS
        );
        // An operator hook row survives ahead of ours.
        let user = "[[hooks]]\nevent = \"stop\"\ncommand = \"echo bye\"\ntimeout_secs = 5\n";
        let text = layer(&cfg, None, Some(user));
        let hooks = parse(&text).hooks;
        assert_eq!(hooks.len(), 2, "{text}");
        assert_eq!(hooks[0].event.wire_name(), "stop");
        assert_eq!(hooks[0].effective_timeout_secs(), 5);
        assert_eq!(hooks[1].event.wire_name(), "permission_request");
    }

    #[test]
    fn idle_recaps_compose_the_engines_idle_task() {
        let cfg: DaemonConfig =
            toml::from_str("sandbox = \"off\"\n[recap]\nidle_minutes = 20\n").unwrap();
        assert_eq!(
            parse(&layer(&cfg, None, None)).context.idle_minutes,
            Some(20)
        );
        // Off (the default) composes nothing, and leaves the operator's
        // row alone; set, Ka Agent's value wins.
        let off = off();
        assert_eq!(compose(&off, None, &no_secret).unwrap(), None);
        let user = "[context]\nidle_minutes = 45\n";
        assert_eq!(
            compose(&off, Some(user), &no_secret).unwrap().as_deref(),
            Some(user)
        );
        assert_eq!(
            parse(&layer(&cfg, None, Some(user))).context.idle_minutes,
            Some(20)
        );
    }
}
