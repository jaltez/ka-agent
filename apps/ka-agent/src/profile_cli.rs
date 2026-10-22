//! `ka-agent profile …`: independent instances under
//! `~/.ka-agent/profiles/<name>/` — each with its own data, config
//! (`config.toml` inside the profile; the default profile's stays at
//! `~/.ka-agent/config.toml`), web port, channels and service units.

use std::path::Path;
use std::process::ExitCode;

use ka_agent_core::layout;
use ka_agent_runtime::paths::Paths;

use crate::ProfileAction;
use crate::admin;
use crate::service;

/// First web port handed to a new profile; the default profile has 8480.
const FIRST_PROFILE_PORT: u16 = 8481;

fn fail(msg: impl std::fmt::Display) -> ExitCode {
    eprintln!("ka-agent: {msg}");
    ExitCode::FAILURE
}

fn paths_for(profile: &str) -> Paths {
    Paths::new(layout::compose_profile_root(&layout::base_dir(), profile))
}

fn exists(profile: &str) -> bool {
    profile == "default" || paths_for(profile).root().is_dir()
}

pub fn profile_action(action: ProfileAction) -> ExitCode {
    if std::env::var_os("KA_AGENT_DATA_DIR").is_some_and(|v| !v.is_empty()) {
        let msg = "KA_AGENT_DATA_DIR pins a single instance; unset it to manage profiles";
        if matches!(action, ProfileAction::List { json: true }) {
            println!("{}", serde_json::json!({"ok": false, "error": msg}));
            return ExitCode::FAILURE;
        }
        return fail(msg);
    }
    match action {
        ProfileAction::List { json } => list(json),
        ProfileAction::Create {
            name,
            from,
            copy_keys,
        } => create(&name, &from, copy_keys),
        ProfileAction::Use { name } => use_profile(&name),
        ProfileAction::Show { name } => show(&name.unwrap_or_else(layout::profile_name)),
        ProfileAction::Remove { name, yes } => remove(&name, yes),
    }
}

fn all_profiles() -> Vec<String> {
    let mut all = layout::list_profiles();
    if !all.iter().any(|p| p == "default") {
        all.insert(0, "default".to_string());
    }
    all
}

fn list(json: bool) -> ExitCode {
    let active = layout::profile_name();
    if json {
        let rows: Vec<serde_json::Value> = all_profiles()
            .into_iter()
            .map(|p| {
                serde_json::json!({
                    "name": p,
                    "active": p == active,
                    "running": admin::is_running(&paths_for(&p)),
                    "web_port": web_port(&layout::config_path_for(&p)),
                    "data": paths_for(&p).root(),
                })
            })
            .collect();
        println!("{}", serde_json::json!({ "ok": true, "profiles": rows }));
        return ExitCode::SUCCESS;
    }
    for p in all_profiles() {
        let mark = if p == active { "*" } else { " " };
        let running = if admin::is_running(&paths_for(&p)) {
            "  running"
        } else {
            ""
        };
        let port = web_port(&layout::config_path_for(&p))
            .map(|n| format!("  web :{n}"))
            .unwrap_or_default();
        println!("{mark} {p}{port}{running}");
    }
    ExitCode::SUCCESS
}

/// Other profiles' configs (not the current one), for clash checks. Empty
/// when the data root is pinned (a single instance outside profiles).
fn sibling_configs() -> Vec<(String, ka_agent_core::DaemonConfig, Paths)> {
    if std::env::var_os("KA_AGENT_DATA_DIR").is_some_and(|v| !v.is_empty()) {
        return Vec::new();
    }
    let me = layout::profile_name();
    all_profiles()
        .into_iter()
        .filter(|p| *p != me)
        .filter_map(|p| {
            let path = layout::config_path_for(&p);
            let cfg = path
                .is_file()
                .then(|| ka_agent_core::DaemonConfig::load(&path).ok())
                .flatten()?;
            let paths = paths_for(&p);
            Some((p, cfg, paths))
        })
        .collect()
}

/// Profiles whose enabled web panel asks for `port`.
pub fn profiles_on_web_port(port: u16) -> Vec<String> {
    sibling_configs()
        .into_iter()
        .filter(|(_, c, _)| c.web.enabled && c.web.port == port)
        .map(|(p, _, _)| p)
        .collect()
}

/// Profiles whose enabled Telegram channel resolves to `token`.
pub fn profiles_polling_token(token: &str) -> Vec<String> {
    sibling_configs()
        .into_iter()
        .filter(|(_, c, paths)| {
            c.telegram.enabled && {
                let name = c
                    .telegram
                    .token_secret
                    .clone()
                    .unwrap_or_else(|| "telegram.bot_token".into());
                ka_agent_core::secret::resolve(&name, &paths.root().join("secrets"))
                    .is_ok_and(|t| t == token)
            }
        })
        .map(|(p, _, _)| p)
        .collect()
}

/// The `[web] port` a config file asks for (8480 when unset or missing).
fn web_port(config: &Path) -> Option<u16> {
    if !config.is_file() {
        return None;
    }
    let text = std::fs::read_to_string(config).ok()?;
    let v: toml::Value = toml::from_str(&text).ok()?;
    Some(
        v.get("web")
            .and_then(|w| w.get("port"))
            .and_then(toml::Value::as_integer)
            .and_then(|n| u16::try_from(n).ok())
            .unwrap_or(8480),
    )
}

/// A web port no profile's config claims yet (8480 counts as taken).
pub fn unclaimed_web_port() -> u16 {
    let taken: Vec<u16> = all_profiles()
        .iter()
        .filter_map(|p| web_port(&layout::config_path_for(p)))
        .chain(std::iter::once(8480))
        .collect();
    free_port(&taken)
}

/// The lowest port from [`FIRST_PROFILE_PORT`] that no profile claims.
fn free_port(taken: &[u16]) -> u16 {
    (FIRST_PROFILE_PORT..u16::MAX)
        .find(|p| !taken.contains(p))
        .unwrap_or(FIRST_PROFILE_PORT)
}

/// A new profile's config text from its source: same model, mode,
/// budgets, timezone; no channels (one bot token cannot be polled by two
/// daemons), no workspace (it follows the new profile's root) and its own
/// web port.
fn derive_config(source: &str, port: u16) -> Result<String, String> {
    let mut doc: toml_edit::DocumentMut = source.parse().map_err(|e| format!("{e}"))?;
    doc.remove("workspace");
    doc.remove("telegram");
    doc.remove("matrix");
    if !doc.contains_table("web") {
        doc["web"] = toml_edit::table();
    }
    doc["web"]["port"] = toml_edit::value(i64::from(port));
    Ok(doc.to_string())
}

fn create(name: &str, from: &str, copy_keys: bool) -> ExitCode {
    if !layout::valid_profile_name(name) {
        return fail(format!("profile name {name:?}: [a-z0-9-], up to 32 chars"));
    }
    if exists(name) {
        return fail(format!("profile {name:?} already exists"));
    }
    if !exists(from) {
        return fail(format!("source profile {from:?} does not exist"));
    }
    let paths = paths_for(name);
    if let Err(e) = paths.ensure_dirs() {
        return fail(format!("{}: {e}", paths.root().display()));
    }

    let port = unclaimed_web_port();
    let source = std::fs::read_to_string(layout::config_path_for(from)).unwrap_or_default();
    let text = match derive_config(&source, port) {
        Ok(t) => t,
        Err(e) => return fail(format!("{from}'s config: {e}")),
    };
    let config = layout::config_path_for(name);
    if let Err(e) = std::fs::write(&config, text) {
        return fail(format!("{}: {e}", config.display()));
    }

    if copy_keys {
        let src = ka_agent_port::catalog::engine_env_path(&paths_for(from).engine_home());
        let dst = ka_agent_port::catalog::engine_env_path(&paths.engine_home());
        if src.is_file() {
            let copied = dst
                .parent()
                .map_or(Ok(()), std::fs::create_dir_all)
                .and_then(|()| std::fs::copy(&src, &dst))
                .and_then(|_| restrict(&dst));
            if let Err(e) = copied {
                return fail(format!("copying keys to {}: {e}", dst.display()));
            }
        }
    }

    println!("created profile {name} (from {from}, web :{port})");
    println!("  data    {}", paths.root().display());
    println!("  config  {}", config.display());
    if !copy_keys {
        println!("  keys    none — run `ka-agent -p {name} setup` or pass --copy-keys");
    }
    println!("  use it  ka-agent -p {name} …   or   ka-agent profile use {name}");
    ExitCode::SUCCESS
}

#[cfg(unix)]
fn restrict(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
}

#[cfg(not(unix))]
fn restrict(_path: &Path) -> std::io::Result<()> {
    Ok(())
}

fn use_profile(name: &str) -> ExitCode {
    if !layout::valid_profile_name(name) || !exists(name) {
        return fail(format!(
            "no profile {name:?} — `ka-agent profile create {name}`"
        ));
    }
    let file = layout::active_profile_file();
    let res = if name == "default" {
        match std::fs::remove_file(&file) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            r => r,
        }
    } else {
        std::fs::create_dir_all(layout::base_dir()).and_then(|()| std::fs::write(&file, name))
    };
    match res {
        Ok(()) => {
            println!("active profile: {name}");
            if std::env::var("KA_AGENT_PROFILE").is_ok_and(|p| !p.is_empty() && p != name) {
                println!("  note: $KA_AGENT_PROFILE is set and still wins in this shell");
            }
            ExitCode::SUCCESS
        }
        Err(e) => fail(format!("{}: {e}", file.display())),
    }
}

fn show(name: &str) -> ExitCode {
    if !exists(name) {
        return fail(format!("no profile {name:?}"));
    }
    let paths = paths_for(name);
    let config = layout::config_path_for(name);
    let n = service::unit_names(name);
    println!("profile   {name}");
    println!("data      {}", paths.root().display());
    println!(
        "config    {}{}",
        config.display(),
        if config.is_file() {
            ""
        } else {
            " (not created)"
        }
    );
    if let Some(port) = web_port(&config) {
        println!("web       http://127.0.0.1:{port}");
    }
    if cfg!(target_os = "macos") {
        println!("service   {}", n.daemon_label);
    } else {
        println!("service   {}", n.daemon_unit);
    }
    println!(
        "daemon    {}",
        if admin::is_running(&paths) {
            "running"
        } else {
            "stopped"
        }
    );
    ExitCode::SUCCESS
}

fn remove(name: &str, yes: bool) -> ExitCode {
    if name == "default" {
        return fail("the default profile cannot be removed");
    }
    if !layout::valid_profile_name(name) || !exists(name) {
        return fail(format!("no profile {name:?}"));
    }
    let paths = paths_for(name);
    if admin::is_running(&paths) {
        return fail(format!(
            "profile {name} is running — `ka-agent -p {name} service uninstall` (or stop it) first"
        ));
    }
    if service::is_installed_for(name) {
        return fail(format!(
            "profile {name} has a background service installed — `ka-agent -p {name} service uninstall` first"
        ));
    }
    if !yes {
        return fail(format!(
            "this deletes {} (memory, sessions, workspace, config) — rerun with --yes",
            paths.root().display()
        ));
    }
    if let Err(e) = std::fs::remove_dir_all(paths.root()) {
        return fail(format!("{}: {e}", paths.root().display()));
    }
    if layout::profile_name() == name {
        let _ = std::fs::remove_file(layout::active_profile_file());
        println!("active profile reset to default");
    }
    println!("removed profile {name}");
    ExitCode::SUCCESS
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    #[test]
    fn derived_config_drops_channels_and_workspace_and_moves_the_port() {
        let src = "# keep me\nmodel = \"m\"\nworkspace = \"/w\"\n[telegram]\nenabled = true\ntoken_secret = \"t\"\n[matrix]\nenabled = true\n[web]\nport = 8480\n[budgets]\ndaily_usd = 1.0\n";
        let out = derive_config(src, 8482).unwrap();
        let v: toml::Value = toml::from_str(&out).unwrap();
        assert_eq!(v["model"].as_str(), Some("m"));
        assert!(v.get("workspace").is_none());
        assert!(v.get("telegram").is_none());
        assert!(v.get("matrix").is_none());
        assert_eq!(v["web"]["port"].as_integer(), Some(8482));
        assert_eq!(v["budgets"]["daily_usd"].as_float(), Some(1.0));
        assert!(out.contains("# keep me"));
        // An empty source still yields a loadable config with its own port.
        let v: toml::Value = toml::from_str(&derive_config("", 8481).unwrap()).unwrap();
        assert_eq!(v["web"]["port"].as_integer(), Some(8481));
    }

    #[test]
    fn free_port_skips_every_claimed_port() {
        assert_eq!(free_port(&[8480]), 8481);
        assert_eq!(free_port(&[8480, 8481, 8483]), 8482);
    }
}
