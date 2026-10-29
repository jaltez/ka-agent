//! The engine layer Ka Agent hands each worker: the operator's optional
//! `ka.engine_layer` file plus what Ka Agent's own config turns on in the
//! engine — web search (`[[search]]`), the fallback model chain
//! (`[fallback]`) and operator MCP servers (`[[mcp]]`). Composed as TOML
//! text, so the worker protocol (`engine_layer_toml`) is unchanged and the
//! worker's security overlay still applies last.

use ka_agent_core::DaemonConfig;

/// Prefix marking a config value that must come from the secret store.
pub const SECRET_PREFIX: &str = "secret:";

/// Compose the layer. `user_layer` is the operator's engine-layer file
/// text; `resolve_secret` maps a secret name to its value (the secret
/// store), so `secret:<name>` values never touch the config file.
pub fn compose(
    cfg: &DaemonConfig,
    user_layer: Option<&str>,
    resolve_secret: &dyn Fn(&str) -> Result<String, String>,
) -> Result<Option<String>, String> {
    let detected = match cfg.sandbox.as_str() {
        "fs" => true,
        "auto" => ka_sandbox::detect_tool().is_some(),
        _ => false,
    };
    // The pinned engine (ka rev 193ad5d, `crates/ka-engine/src/voice.rs`
    // ~2720-2760) poses a "sandbox grant for `…`" ask even when the
    // permission gate returned `Gate::Allow` — so `mode = "allow"` and
    // `grants.full` must keep the sandbox off entirely, or bash would
    // still ask outside the workspace.
    let sandbox_on = detected && cfg.mode != "allow" && !cfg.grants.full;
    // Allow-dir grants ride the sandbox allowlist and engine rules.
    let grant_dirs: Vec<&ka_agent_core::config::PathGrant> = cfg
        .grants
        .path
        .iter()
        .filter(|g| g.access == ka_agent_core::config::Access::Allow)
        .collect();
    let has_extras = cfg.search.is_some()
        || !cfg.fallback.is_empty()
        || !cfg.mcp.is_empty()
        || cfg.approvals.auto_review
        || cfg.effort.is_some()
        || sandbox_on
        || cfg.budgets.daily_usd.is_some()
        || cfg
            .hooks
            .iter()
            .any(|h| ka_agent_core::config::ENGINE_HOOK_EVENTS.contains(&h.on.as_str()));
    if !has_extras {
        return Ok(user_layer.map(str::to_string));
    }
    let mut doc: toml::Table = match user_layer {
        Some(text) => text.parse().map_err(|e| format!("engine layer: {e}"))?,
        None => toml::Table::new(),
    };

    if let Some(search) = &cfg.search {
        let mut row = toml::Table::new();
        row.insert("provider".into(), search.provider.clone().into());
        row.insert("api_key_env".into(), search.key_env().into());
        if let Some(base) = &search.base_url {
            row.insert("base_url".into(), base.clone().into());
        }
        // The engine uses the first entry; Ka Agent's choice wins.
        doc.insert("search".into(), toml::Value::Array(vec![row.into()]));
    }

    if !cfg.fallback.is_empty() {
        let mut fb = toml::Table::new();
        fb.insert(
            "models".into(),
            toml::Value::Array(cfg.fallback.iter().cloned().map(Into::into).collect()),
        );
        doc.insert("fallback".into(), fb.into());
    }

    if cfg.approvals.auto_review {
        let guards = doc
            .entry("guards")
            .or_insert_with(|| toml::Table::new().into());
        if let Some(g) = guards.as_table_mut() {
            g.insert("auto_review".into(), true.into());
        }
    }

    // Engine hooks: the engine runs them with its own semantics (sh -c,
    // 30 s cap, exit 2 blocks, stdout steers). Agent-event hooks are the
    // runtime's (HookSurface), never the engine's.
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
    // raises becomes an approval card the operator already knows.
    if let Some(cap) = cfg.budgets.daily_usd {
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
            servers.push(mcp_row(m, resolve_secret)?.into());
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
        let search = engine.effective_search().expect("search on");
        assert_eq!(search.provider, "tavily");
        assert_eq!(search.api_key_env, "TAVILY_API_KEY");
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
        assert_eq!(engine.tools.mcp.discovery.as_deref(), Some("lazy"));
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
