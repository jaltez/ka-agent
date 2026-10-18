//! `ka-agent model` / `ka-agent models`: inspect and switch the model.

use std::path::PathBuf;

use ka_agent_port::catalog;
use ka_agent_runtime::paths::Paths;

use crate::admin;

pub struct ModelArgs {
    pub selector: Option<String>,
    pub config: PathBuf,
}

pub async fn model(args: ModelArgs) -> std::process::ExitCode {
    let paths = Paths::new(Paths::default_root());
    let cfg = ka_agent_core::DaemonConfig::load(&args.config).unwrap_or_default();
    match args.selector {
        None => {
            let mut lines = vec![match &cfg.model {
                Some(m) => format!("model: {m}"),
                None => "model: none".to_string(),
            }];
            let sel = cfg
                .model
                .as_deref()
                .and_then(|m| catalog::validate_selector(m).ok());
            if let Some(sel) = sel {
                let env = catalog::read_env_file(&catalog::engine_env_path(&paths.engine_home()));
                match catalog::key_state(&sel, &env) {
                    catalog::KeyState::Present => {
                        if let Some(env) = sel.key_env {
                            lines.push(format!("✓ {env} set"));
                        }
                    }
                    catalog::KeyState::Missing(env) => {
                        lines.push(format!("✗ {env} missing — ka-agent key set {env}"))
                    }
                    catalog::KeyState::Keyless => {}
                }
            }
            lines.push("switch: ka-agent model provider/model · list: ka-agent models".into());
            for l in lines {
                println!("{l}");
            }
            std::process::ExitCode::SUCCESS
        }
        Some(selector) => match catalog::validate_selector(&selector) {
            Ok(info) => {
                match ka_agent_core::config_file::set(&args.config, "model", &info.id) {
                    Ok(_) => {}
                    Err(e) => {
                        eprintln!("ka-agent: {e}");
                        return std::process::ExitCode::FAILURE;
                    }
                }
                // Key warning (the selector is saved regardless).
                if let Some(env) = &info.key_env {
                    let envs =
                        catalog::read_env_file(&catalog::engine_env_path(&paths.engine_home()));
                    if !envs.contains_key(env) && std::env::var(env).is_err() {
                        println!("✗ {env} missing — ka-agent key set {env}");
                    }
                }
                if admin::is_running(&paths) {
                    match admin::request(&paths, &serde_json::json!({"op": "reload"})) {
                        Ok(_) => println!("applied live — the next message uses {}", info.id),
                        Err(e) => {
                            println!("saved, but the running agent rejected it: {e}");
                            return std::process::ExitCode::FAILURE;
                        }
                    }
                } else {
                    println!("saved — takes effect when Ka Agent starts");
                }
                std::process::ExitCode::SUCCESS
            }
            Err(e) => {
                eprintln!("ka-agent: {e}");
                std::process::ExitCode::FAILURE
            }
        },
    }
}

pub fn models(provider: Option<String>) -> std::process::ExitCode {
    let paths = Paths::new(Paths::default_root());
    let envs = catalog::read_env_file(&catalog::engine_env_path(&paths.engine_home()));
    let list: Vec<_> = match &provider {
        Some(name) => catalog::provider(name).into_iter().collect(),
        None => catalog::providers(),
    };
    if list.is_empty() {
        eprintln!("ka-agent: unknown provider {provider:?} — ka-agent models lists them");
        return std::process::ExitCode::FAILURE;
    }
    for p in list {
        let key = match p.key_env {
            Some(e) if envs.contains_key(e) || std::env::var(e).is_ok() => "✓",
            Some(_) => "✗",
            None => "n/a",
        };
        println!("{}  key {key}  {}", p.name, p.note);
        let mut models = Vec::new();
        if let Some(d) = catalog::default_model(p.name) {
            models.push(d.to_string());
        }
        for m in catalog::catalog_models(p.name) {
            if !models.contains(&m) {
                models.push(m);
            }
        }
        let cap = 20;
        let more = models.len().saturating_sub(cap);
        for m in models.into_iter().take(cap) {
            println!("  {m}");
        }
        if more > 0 {
            println!("  … and {more} more");
        }
    }
    std::process::ExitCode::SUCCESS
}
