//! `ka-agent config`: inspect and change the config file, with live
//! reload when the daemon runs. Format-preserving: comments and key order
//! survive every edit.

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

fn not_created(path: &Path) -> String {
    format!("{} (not created yet — run ka-agent setup)", path.display())
}

/// The wording for "the change waits for a restart".
fn restart_hint(_paths: &Paths) -> &'static str {
    if crate::service::is_installed() {
        "run `ka-agent restart`"
    } else {
        "stop and start `ka-agent run` again"
    }
}

pub fn apply_live(paths: &Paths, _config: &PathBuf) -> std::process::ExitCode {
    if !admin::is_running(paths) {
        println!("takes effect when Ka Agent starts");
        return std::process::ExitCode::SUCCESS;
    }
    match admin::request(paths, &serde_json::json!({"op": "reload"})) {
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
            if !applied.is_empty() {
                println!("applied live: {}", applied.join(", "));
            }
            if !restart.is_empty() {
                println!(
                    "restart needed for: {} — {}",
                    restart.join(", "),
                    restart_hint(paths)
                );
            }
            if applied.is_empty() && restart.is_empty() {
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

pub fn config(command: ConfigCommand, config: PathBuf) -> std::process::ExitCode {
    let paths = Paths::new(Paths::default_root());
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
        ConfigCommand::Get { key } if ka_agent_core::config_keys::lookup(&key).is_none() => {
            eprintln!("ka-agent: {}", unknown_key(&key));
            std::process::ExitCode::FAILURE
        }
        ConfigCommand::Get { key } => match ka_agent_core::config_file::get(&config, &key) {
            Ok(Some(value)) => {
                println!("{value}");
                std::process::ExitCode::SUCCESS
            }
            Ok(None) => {
                println!("{key} is not set (default applies)");
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
            match ka_agent_core::config_file::set(&config, &key, &value) {
                Ok(_) => {
                    let rendered = ka_agent_core::config_file::get(&config, &key)
                        .ok()
                        .flatten();
                    match rendered {
                        Some(v) => println!("saved {key} = {v}"),
                        None => println!("saved {key}"),
                    }
                    apply_live(&paths, &config)
                }
                Err(e) => {
                    eprintln!("ka-agent: {e}");
                    std::process::ExitCode::FAILURE
                }
            }
        }
        ConfigCommand::Unset { key } if ka_agent_core::config_keys::lookup(&key).is_none() => {
            eprintln!("ka-agent: {}", unknown_key(&key));
            std::process::ExitCode::FAILURE
        }
        ConfigCommand::Unset { key } => match ka_agent_core::config_file::unset(&config, &key) {
            Ok(_) => {
                println!("removed {key}");
                apply_live(&paths, &config)
            }
            Err(e) => {
                eprintln!("ka-agent: {e}");
                std::process::ExitCode::FAILURE
            }
        },
        ConfigCommand::Schema { json } => schema(json),
        ConfigCommand::Edit => edit(&config, &paths),
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
/// atomically installed.
fn edit(config: &PathBuf, paths: &Paths) -> std::process::ExitCode {
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
                let doc = ka_agent_core::config_file::read_doc(&tmp).and_then(|d| {
                    ka_agent_core::config_file::write_doc(config, &d)?;
                    Ok(d)
                });
                let _ = std::fs::remove_file(&tmp);
                return match doc {
                    Ok(_) => apply_live(paths, config),
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
