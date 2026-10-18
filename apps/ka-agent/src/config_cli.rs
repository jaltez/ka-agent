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
    Edit,
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

fn apply_live(paths: &Paths, _config: &PathBuf) -> std::process::ExitCode {
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
        ConfigCommand::Set { key, value } => {
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
        ConfigCommand::Edit => edit(&config, &paths),
    }
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
