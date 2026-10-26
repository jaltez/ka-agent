//! Plugin tools: verified manifests under `<data>/plugins`, each call
//! scope-granted and (for writes) intent-logged, then run in the
//! bubblewrap sandbox on a blocking thread. Split from the old MCP bridge
//! (moves only).

use std::path::PathBuf;

use serde_json::{Value, json};

use super::{Effect, ToolCtx};

/// One loadable plugin: its validated manifest and installed root. Loaded
/// once per bridge start; a digest mismatch or invalid manifest disables the
/// plugin with a visible reason (never half-loaded, SEED.md §6.5).
pub struct LoadedPlugin {
    pub manifest: ka_agent_plugins::Manifest,
    pub root: PathBuf,
}

/// Enabled plugin tools as MCP tool definitions (generic v1 argument
/// schema: the manifest declares names, effects and scopes).
pub async fn list(ctx: &ToolCtx) -> Vec<Value> {
    let root = ctx.paths.root().to_path_buf();
    tokio::task::spawn_blocking(move || {
        let (plugins, refused) = load_plugins_at(&root);
        for (id, reason) in &refused {
            eprintln!("kagent: plugin {id} disabled: {reason}");
        }
        plugins
            .iter()
            .flat_map(|p| {
                p.manifest.tools.iter().map(|t| {
                    json!({
                        "name": t.name.clone(),
                        "description": format!("plugin {} · {}", p.manifest.plugin.id, t.name),
                        "inputSchema": {"type": "object", "additionalProperties": true},
                    })
                })
            })
            .collect::<Vec<_>>()
    })
    .await
    .unwrap_or_default()
}

/// Disabled plugins, for the daemon log at startup.
pub async fn refused(ctx: &ToolCtx) -> Vec<(String, String)> {
    let root = ctx.paths.root().to_path_buf();
    tokio::task::spawn_blocking(move || load_plugins_at(&root).1)
        .await
        .unwrap_or_default()
}

pub(crate) async fn call(ctx: &ToolCtx, name: &str, args: Value) -> Result<String, String> {
    let root = ctx.paths.root().to_path_buf();
    let store = std::sync::Arc::clone(&ctx.store);
    let name = name.to_string();
    let grants = crate::grants::Grants::load(&ctx.store).map_err(|e| format!("grants: {e}"))?;
    tokio::task::spawn_blocking(move || {
        let (plugins, _) = load_plugins_at(&root);
        for plugin in &plugins {
            if let Some(tool) = plugin.manifest.tool(&name) {
                return call_plugin_at(&root, &store, plugin, tool, &args, &grants);
            }
        }
        Err(format!("unknown tool: {name}"))
    })
    .await
    .map_err(|e| format!("plugin task: {e}"))?
}

pub async fn effect_of(ctx: &ToolCtx, name: &str) -> Effect {
    let root = ctx.paths.root().to_path_buf();
    let name = name.to_string();
    tokio::task::spawn_blocking(move || {
        let (plugins, _) = load_plugins_at(&root);
        for plugin in &plugins {
            if let Some(tool) = plugin.manifest.tool(&name) {
                return if tool.effect == "read" {
                    Effect::Read
                } else {
                    Effect::ExternalWrite
                };
            }
        }
        Effect::ExternalWrite
    })
    .await
    .unwrap_or(Effect::ExternalWrite)
}

/// Stable short digest of the call arguments (the intent's idempotency
/// key material).
fn sha256_of(args: &Value) -> u64 {
    use std::hash::{Hash, Hasher};
    let text = serde_json::to_string(args).unwrap_or_default();
    let mut h = std::collections::hash_map::DefaultHasher::new();
    text.hash(&mut h);
    h.finish()
}

/// Engine-visible names of plugin tools whose effect never asks (the
/// approval profile), computed synchronously for host start-up.
pub fn auto_allow_names(paths: &ka_agent_runtime::paths::Paths) -> Vec<String> {
    let (plugins, _) = load_plugins_at(paths.root());
    plugins
        .iter()
        .flat_map(|p| {
            p.manifest
                .tools
                .iter()
                .filter(|t| t.effect == "read")
                .map(|t| format!("kagent__{}", t.name))
        })
        .collect()
}

/// `load_plugins` with the data root passed in (the daemon has Paths).
pub(crate) fn load_plugins_at(
    root: &std::path::Path,
) -> (Vec<LoadedPlugin>, Vec<(String, String)>) {
    let mut ok = Vec::new();
    let mut refused = Vec::new();
    let plugins_root = root.join("plugins");
    let lock = match ka_agent_plugins::Lockfile::load(&plugins_root.join("installed.json")) {
        Ok(l) => l,
        Err(e) => {
            refused.push(("<lockfile>".into(), e.to_string()));
            return (ok, refused);
        }
    };
    for record in &lock.plugins {
        if !record.enabled {
            continue;
        }
        let dir = plugins_root.join(&record.id);
        let mut refuse = |reason: String| refused.push((record.id.clone(), reason));
        match ka_agent_plugins::Manifest::load(&dir.join(ka_agent_plugins::MANIFEST_NAME)) {
            Err(e) => refuse(format!("manifest: {e}")),
            Ok(manifest) => match ka_agent_plugins::tree_digest(&dir) {
                Err(e) => refuse(format!("digest: {e}")),
                Ok(digest) => {
                    if digest != record.digest {
                        refuse(
                            "installed files no longer match the pinned digest \
                                   — reinstall the plugin or run `ka-agent plugin rm`"
                                .to_string(),
                        );
                    } else {
                        ok.push(LoadedPlugin {
                            manifest,
                            root: dir,
                        });
                    }
                }
            },
        }
    }
    (ok, refused)
}

/// `call_plugin` with explicit paths (no process-env lookups).
fn call_plugin_at(
    root: &std::path::Path,
    store: &ka_agent_store::Store,
    plugin: &LoadedPlugin,
    tool: &ka_agent_plugins::manifest::ToolSection,
    args: &Value,
    grants: &crate::grants::Grants,
) -> Result<String, String> {
    for scope in &tool.scopes {
        if !grants.check(scope) {
            return Err(format!(
                "no grant for {scope} — ask the operator to run: ka-agent grant add {scope}"
            ));
        }
    }
    let secrets_dir = root.join("secrets");
    let mut secrets = std::collections::HashMap::new();
    for name in &plugin.manifest.requires.secrets {
        let value = ka_agent_core::secret::resolve(name, &secrets_dir)
            .map_err(|e| format!("secret {name:?}: {e}"))?;
        secrets.insert(name.clone(), value);
    }
    let mut intent: Option<String> = None;
    if tool.effect != "read" {
        let now = ka_agent_core::now_ms();
        let id = ka_agent_core::IntentId::generate();
        let row = ka_agent_store::IntentRow {
            id: id.to_string(),
            tool: tool.name.clone(),
            args_hash: format!("{:x}", sha256_of(args)),
            status: "executing".to_string(),
            external_ref: None,
            summary: Some(format!(
                "plugin {} · {}",
                plugin.manifest.plugin.id, tool.name
            )),
            created_ms: now,
            updated_ms: now,
        };
        store.insert_intent(&row).ok();
        intent = Some(id.to_string());
    }
    let sandbox = ka_agent_plugins::Sandbox {
        plugin_root: plugin.root.clone(),
        command: plugin.manifest.plugin.entry.command.clone(),
        args: plugin.manifest.plugin.entry.args.clone(),
        secrets,
    };
    let answer = sandbox.call(&tool.name, args).map_err(|e| {
        if let Some(id) = intent.as_deref() {
            store.update_intent_status(id, "failed", None).ok();
        }
        format!("plugin {}: {e}", plugin.manifest.plugin.id)
    })?;
    if let Some(id) = intent.as_deref() {
        store.update_intent_status(id, "done", None).ok();
    }
    Ok(answer)
}

/// The data root the plugin loader resolves from (kept for `plugin list`).
pub fn data_root_env() -> Result<PathBuf, String> {
    std::env::var_os("KA_AGENT_DATA_DIR")
        .map(PathBuf::from)
        .ok_or_else(|| "KA_AGENT_DATA_DIR not set".to_string())
}
