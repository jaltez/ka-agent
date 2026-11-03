//! `ka-agent config`: inspect and change the config file, with live
//! reload when the daemon runs. Format-preserving: comments and key order
//! survive every edit, and every write keeps the previous file one
//! generation as `<config>.bak`.

use std::io::IsTerminal;
use std::path::{Path, PathBuf};

use ka_agent_runtime::paths::Paths;

use crate::admin;
use crate::prompt;

pub enum ConfigCommand {
    Path,
    Show,
    Get { key: String },
    Set { key: String, value: String },
    Unset { key: String },
    Schema { json: bool },
    Edit,
}

/// `unknown key \`x\` — did you mean \`y\`?` against the registry.
fn unknown_key(key: &str) -> String {
    match ka_agent_core::config_keys::suggest(key) {
        Some(near) => format!("unknown key `{key}` — did you mean `{near}`?"),
        None => format!("unknown key `{key}` — `ka-agent config schema` lists every key"),
    }
}

/// A leaf key from the registry, or a whole registered section (`search`,
/// `voice`, …, and the table entries `mcp`/`hooks`): `config get` and
/// `config unset` accept both. `config set` stays leaf-only.
fn registered(key: &str) -> bool {
    ka_agent_core::config_keys::lookup(key).is_some() || ka_agent_core::config_keys::is_section(key)
}

fn not_created(path: &Path) -> String {
    format!("{} (not created yet — run ka-agent setup)", path.display())
}

/// Which profile's config file this is: `<base>/config.toml` is the
/// default profile's, `<base>/profiles/<p>/config.toml` is `p`'s.
/// `None` for a path outside the profile tree (a `-c` file of one's own).
fn config_profile(config: &Path) -> Option<String> {
    let base = ka_agent_core::layout::base_dir();
    if config == base.join("config.toml") {
        return Some("default".to_string());
    }
    let name = config
        .parent()?
        .strip_prefix(base.join("profiles"))
        .ok()?
        .to_str()?;
    ka_agent_core::layout::valid_profile_name(name).then(|| name.to_string())
}

/// The instance that actually runs `config`: the pinned
/// `$KA_AGENT_DATA_DIR` root, else the profile owning the file, else the
/// ambient one. Advice and live reloads must name THIS instance — never
/// the ambient profile's daemon when the file belongs to another (D7).
fn target_paths(config: &Path) -> Paths {
    if let Some(dir) = std::env::var_os("KA_AGENT_DATA_DIR")
        && !dir.is_empty()
    {
        return Paths::new(std::path::PathBuf::from(dir));
    }
    match config_profile(config) {
        Some(profile) => Paths::new(ka_agent_core::layout::compose_profile_root(
            &ka_agent_core::layout::base_dir(),
            &profile,
        )),
        None => Paths::new(Paths::default_root()),
    }
}

/// The wording for "the change waits for a restart": it names the unit
/// that runs the edited config — `ka-agent -p <profile> restart` for
/// another profile's file, and never a profile unit at all when the data
/// root is pinned with `$KA_AGENT_DATA_DIR` (that daemon was started
/// outside the units, or under one we cannot name from here).
fn restart_hint(config: &Path) -> String {
    if std::env::var_os("KA_AGENT_DATA_DIR").is_some_and(|v| !v.is_empty()) {
        return format!(
            "stop and start the daemon at {} again",
            Paths::default_root().display()
        );
    }
    let Some(profile) = config_profile(config) else {
        return "stop and start `ka-agent run` again".to_string();
    };
    let (restart, run) = if profile == ka_agent_core::layout::profile_name() {
        ("ka-agent restart".to_string(), "ka-agent run".to_string())
    } else {
        (
            format!("ka-agent -p {profile} restart"),
            format!("ka-agent -p {profile} run"),
        )
    };
    if crate::service::is_installed_for(&profile) {
        format!("run `{restart}`")
    } else {
        format!("stop and start `{run}` again")
    }
}

/// Reload the daemon that runs `config` (if one answers on that root's
/// admin socket) and report what the reload changed. Worker-class keys
/// are labeled with their registry wording — "applied (next turn)" —
/// because a running turn finishes on the old settings (D6).
pub fn apply_live(config: &Path) -> std::process::ExitCode {
    let paths = target_paths(config);
    if !admin::is_running(&paths) {
        println!("takes effect when Ka Agent starts");
        return std::process::ExitCode::SUCCESS;
    }
    match admin::request(&paths, &serde_json::json!({"op": "reload"})) {
        Ok(v) => {
            let applied: Vec<String> = v["report"]["applied"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(|x| x.as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default();
            let restart: Vec<String> = v["report"]["restart_required"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(|x| x.as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default();
            // D6: "applied live" only for Apply::Live; Worker keys shape
            // the engine the NEXT turn's worker carries.
            let (live, next_turn): (Vec<String>, Vec<String>) =
                applied.into_iter().partition(|key| {
                    !ka_agent_core::config_keys::lookup(key)
                        .is_some_and(|k| k.apply == ka_agent_core::config_keys::Apply::Worker)
                });
            if !live.is_empty() {
                println!("applied live: {}", live.join(", "));
            }
            if !next_turn.is_empty() {
                println!("applied (next turn): {}", next_turn.join(", "));
            }
            if !restart.is_empty() {
                println!(
                    "restart needed for: {} — {}",
                    restart.join(", "),
                    restart_hint(config)
                );
            }
            if live.is_empty() && next_turn.is_empty() && restart.is_empty() {
                println!("nothing changed for the running agent");
            }
            std::process::ExitCode::SUCCESS
        }
        Err(e) => {
            println!("the running agent rejected the new config: {e}");
            std::process::ExitCode::FAILURE
        }
    }
}

/// `model` and `fallback` name catalog models: refuse unknown selectors
/// before anything is written (the schema alone cannot know the catalog).
fn check_selectors(key: &str, value: &str) -> Result<(), String> {
    let selectors: Vec<String> = match key {
        "model" => vec![value.trim().trim_matches('"').to_string()],
        "fallback" => toml::from_str::<toml::Table>(&format!("v = {value}"))
            .ok()
            .and_then(|t| t.get("v").and_then(|v| v.as_array()).cloned())
            .map(|a| {
                a.iter()
                    .filter_map(|x| x.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default(),
        _ => return Ok(()),
    };
    for sel in &selectors {
        ka_agent_port::catalog::validate_selector(sel).map_err(|e| format!("{key}: {e}"))?;
    }
    Ok(())
}

/// `ka.engine_layer` must name a readable TOML file: the worker parses
/// it at spawn time, so an unreadable or broken layer would otherwise
/// surface only as the NEXT turn's engine error (D5).
fn check_engine_layer(value: &str) -> Result<(), String> {
    let path = ka_agent_core::expand_home(value.trim().trim_matches('"'));
    let text = std::fs::read_to_string(&path)
        .map_err(|e| format!("ka.engine_layer: cannot read {}: {e}", path.display()))?;
    toml::from_str::<toml::Table>(&text)
        .map_err(|e| format!("ka.engine_layer: {}: not valid TOML: {e}", path.display()))?;
    Ok(())
}

pub fn config(command: ConfigCommand, config: PathBuf) -> std::process::ExitCode {
    match command {
        ConfigCommand::Path => {
            if config.exists() {
                println!("{}", config.display());
            } else {
                println!("{}", not_created(&config));
            }
            std::process::ExitCode::SUCCESS
        }
        ConfigCommand::Show => match std::fs::read_to_string(&config) {
            Ok(text) => {
                print!("{text}");
                std::process::ExitCode::SUCCESS
            }
            Err(_) => {
                eprintln!("ka-agent: {}", not_created(&config));
                std::process::ExitCode::FAILURE
            }
        },
        ConfigCommand::Get { key } if !registered(&key) => {
            eprintln!("ka-agent: {}", unknown_key(&key));
            std::process::ExitCode::FAILURE
        }
        ConfigCommand::Get { key } => match ka_agent_core::config_file::get(&config, &key) {
            Ok(Some(value)) => {
                println!("{value}");
                std::process::ExitCode::SUCCESS
            }
            Ok(None) if ka_agent_core::config_keys::is_section(&key) => {
                println!("{key} is not set (section absent)");
                std::process::ExitCode::SUCCESS
            }
            Ok(None) => {
                // D9: name the default that will apply, from the
                // registry, so the operator need not run `config schema`.
                let default = ka_agent_core::config_keys::lookup(&key)
                    .map(|k| k.default)
                    .unwrap_or("unset");
                println!("{key} is not set (default: {default})");
                std::process::ExitCode::SUCCESS
            }
            Err(e) => {
                eprintln!("ka-agent: {e}");
                std::process::ExitCode::FAILURE
            }
        },
        ConfigCommand::Set { key, value: _ }
            if ka_agent_core::config_keys::lookup(&key).is_none() =>
        {
            eprintln!("ka-agent: {}", unknown_key(&key));
            std::process::ExitCode::FAILURE
        }
        ConfigCommand::Set { key, value } => {
            if let Err(e) = check_selectors(&key, &value) {
                eprintln!("ka-agent: {e}");
                return std::process::ExitCode::FAILURE;
            }
            if key == "ka.engine_layer"
                && let Err(e) = check_engine_layer(&value)
            {
                eprintln!("ka-agent: {e}");
                return std::process::ExitCode::FAILURE;
            }
            match ka_agent_core::config_file::set(&config, &key, &value) {
                Ok(_) => {
                    let rendered = ka_agent_core::config_file::get(&config, &key)
                        .ok()
                        .flatten();
                    match rendered {
                        Some(v) => println!("saved {key} = {v}"),
                        None => println!("saved {key}"),
                    }
                    apply_live(&config)
                }
                Err(e) => {
                    eprintln!("ka-agent: {e}");
                    std::process::ExitCode::FAILURE
                }
            }
        }
        ConfigCommand::Unset { key } if !registered(&key) => {
            eprintln!("ka-agent: {}", unknown_key(&key));
            std::process::ExitCode::FAILURE
        }
        ConfigCommand::Unset { key } => {
            // D1: unsetting a table key (`mcp`, `hooks`, `grants.path`,
            // or a section covering one) deletes every row at once —
            // show them and get an explicit yes first.
            if !confirm_table_unset(&config, &key) {
                return std::process::ExitCode::FAILURE;
            }
            match ka_agent_core::config_file::unset(&config, &key) {
                Ok(_) => {
                    println!("removed {key}");
                    apply_live(&config)
                }
                Err(e) => {
                    eprintln!("ka-agent: {e}");
                    std::process::ExitCode::FAILURE
                }
            }
        }
        ConfigCommand::Schema { json } => schema(json),
        ConfigCommand::Edit => edit(&config),
    }
}

/// `config schema`: one row per registered key, or the registry as JSON.
fn schema(json: bool) -> std::process::ExitCode {
    use ka_agent_core::config_keys::{Apply, KEYS, Kind};
    if json {
        let rows: Vec<serde_json::Value> = KEYS
            .iter()
            .map(|k| {
                serde_json::json!({
                    "path": k.path,
                    "type": k.kind.label(),
                    "default": k.default,
                    "applies": k.apply.label(),
                    "web": k.web,
                    "doc": k.doc,
                })
            })
            .collect();
        println!(
            "{}",
            serde_json::to_string_pretty(&rows).unwrap_or_default()
        );
        return std::process::ExitCode::SUCCESS;
    }
    for k in KEYS {
        let kind = match k.kind {
            Kind::Int { min, max } => format!("int {min}..={max}"),
            Kind::Float { min } => format!("float ≥ {min}"),
            Kind::Enum(vals) => format!("enum {}", vals.join("|")),
            other => other.label().to_string(),
        };
        println!(
            "{} · {} · default {} · applies {} · {}",
            k.path,
            kind,
            k.default,
            k.apply.label(),
            k.doc
        );
        let _ = Apply::Live;
    }
    std::process::ExitCode::SUCCESS
}

/// `$VISUAL`/`$EDITOR` (else nano/notepad) on a copy, validated, then
/// atomically installed. Prints which registered keys the edit changed
/// (D10) — the editor may touch many at once.
fn edit(config: &Path) -> std::process::ExitCode {
    let tmp = config.with_extension("edit.toml");
    loop {
        if std::fs::copy(config, &tmp).is_err() {
            // Fresh edit of a not-yet-created file: start from an empty doc.
            let _ = std::fs::write(&tmp, "");
        }
        let editor = std::env::var("VISUAL")
            .or_else(|_| std::env::var("EDITOR"))
            .unwrap_or_else(|_| {
                if cfg!(windows) {
                    "notepad".to_string()
                } else {
                    "nano".to_string()
                }
            });
        let status = std::process::Command::new(&editor).arg(&tmp).status();
        match status {
            Ok(s) if s.success() => {}
            _ => {
                eprintln!("ka-agent: editor {editor:?} failed");
                let _ = std::fs::remove_file(&tmp);
                return std::process::ExitCode::FAILURE;
            }
        }
        let text = std::fs::read_to_string(&tmp).unwrap_or_default();
        match toml::from_str::<ka_agent_core::DaemonConfig>(&text).map_err(|e| {
            ka_agent_core::ConfigError::Parse {
                path: tmp.clone(),
                message: e.to_string(),
            }
        }) {
            Ok(_) => {
                let old = std::fs::read_to_string(config)
                    .ok()
                    .and_then(|t| toml::from_str::<ka_agent_core::DaemonConfig>(&t).ok());
                let doc = ka_agent_core::config_file::read_doc(&tmp).and_then(|d| {
                    let new = ka_agent_core::config_file::write_doc(config, &d)?;
                    Ok((d, new))
                });
                let _ = std::fs::remove_file(&tmp);
                return match doc {
                    Ok((_, new)) => {
                        if let Some(old) = old {
                            let changed = ka_agent_core::config_keys::changed(&old, &new);
                            if changed.is_empty() {
                                println!("config unchanged");
                            } else {
                                let names: Vec<&str> = changed.iter().map(|k| k.path).collect();
                                println!("changed: {}", names.join(", "));
                            }
                        }
                        apply_live(config)
                    }
                    Err(e) => {
                        eprintln!("ka-agent: {e}");
                        std::process::ExitCode::FAILURE
                    }
                };
            }
            Err(e) => {
                println!("invalid config: {e}");
                let options = vec!["Edit again".to_string(), "Discard changes".to_string()];
                match prompt::choice("", &options, 0) {
                    Ok(0) => continue,
                    _ => {
                        let _ = std::fs::remove_file(&tmp);
                        println!("changes discarded");
                        return std::process::ExitCode::FAILURE;
                    }
                }
            }
        }
    }
}

/// Guard for `config unset` on an array-of-tables key (`mcp`, `hooks`,
/// `grants.path`) or a section covering one: removing it deletes every
/// row at once, so list them and get an explicit yes (D1). Without a
/// terminal — scripts, pipes — refuse and name the per-row commands
/// instead; a silent bulk delete is exactly the foot-gun.
fn confirm_table_unset(config: &Path, key: &str) -> bool {
    let rows = table_rows(config, key);
    if rows.is_empty() {
        return true; // nothing set: the unset is a no-op
    }
    println!("{key} is about to lose {} row(s):", rows.len());
    for row in rows.iter().take(10) {
        println!("  {row}");
    }
    if rows.len() > 10 {
        println!("  … and {} more", rows.len() - 10);
    }
    if !std::io::stdin().is_terminal() {
        eprintln!(
            "ka-agent: refusing to remove {key} rows without a terminal — remove them one by one (`ka-agent mcp rm <name>`, `ka-agent grant path rm <dir>`) or edit the file with `ka-agent config edit`"
        );
        return false;
    }
    prompt::yes_no(&format!("Remove all {key} rows?"), false).unwrap_or(false)
}

/// The rows a table key (or a section covering one) holds, one line
/// each, as `k = v, k = v` — what `config unset` would delete.
fn table_rows(config: &Path, key: &str) -> Vec<String> {
    use ka_agent_core::config_keys::{KEYS, Kind};
    let targets: Vec<&str> = match KEYS.iter().find(|k| k.path == key && k.kind == Kind::Table) {
        Some(k) => vec![k.path],
        None => KEYS
            .iter()
            .filter(|k| k.kind == Kind::Table && k.path.starts_with(&format!("{key}.")))
            .map(|k| k.path)
            .collect(),
    };
    let Ok(doc) = ka_agent_core::config_file::read_doc(config) else {
        return Vec::new();
    };
    let mut rows = Vec::new();
    for target in targets {
        let mut item: Option<&toml_edit::Item> = Some(doc.as_item());
        for seg in target.split('.') {
            item = item
                .and_then(toml_edit::Item::as_table_like)
                .and_then(|t| t.get(seg));
        }
        if let Some(tables) = item.and_then(toml_edit::Item::as_array_of_tables) {
            for table in tables.iter() {
                let pairs: Vec<String> = table.iter().map(|(k, v)| format!("{k} = {v}")).collect();
                rows.push(pairs.join(", "));
            }
        }
    }
    rows
}
