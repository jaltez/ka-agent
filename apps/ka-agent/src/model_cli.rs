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

pub struct ModelsArgs {
    pub provider: Option<String>,
    /// Refetch the live registry even when the cache is fresh.
    pub refresh: bool,
    /// Probe target: `None` = list only, `Some("")` = the configured
    /// model, `Some("vendor/model")` = that one.
    pub probe: Option<String>,
    pub config: PathBuf,
}

pub fn models(args: ModelsArgs) -> std::process::ExitCode {
    let paths = Paths::new(Paths::default_root());
    if let Some(target) = args.probe.as_deref() {
        return probe(&args.config, target);
    }
    let envs = catalog::read_env_file(&catalog::engine_env_path(&paths.engine_home()));
    let list: Vec<_> = match &args.provider {
        Some(name) => catalog::provider(name).into_iter().collect(),
        None => catalog::providers(),
    };
    if list.is_empty() {
        eprintln!(
            "ka-agent: unknown provider {:?} — ka-agent models lists them",
            args.provider
        );
        return std::process::ExitCode::FAILURE;
    }
    // The live models.dev registry, merged over the pinned catalog:
    // `None` (offline, no cache) keeps the output exactly as before.
    let reg = crate::models_live::load(paths.root(), args.refresh);
    for p in list {
        let key = match p.key_env {
            Some(e) if envs.contains_key(e) || std::env::var(e).is_ok() => "✓",
            Some(_) => "✗",
            None => "n/a",
        };
        println!("{}  key {key}  {}", p.name, p.note);
        let models = crate::models_live::merged(p.name, reg.as_ref());
        let cap = 20;
        let more = models.len().saturating_sub(cap);
        for m in models.into_iter().take(cap) {
            println!("  {m}");
        }
        if more > 0 {
            println!("  … and {more} more");
        }
        if let Some(foot) = crate::models_live::footer(reg.as_ref(), p.name) {
            println!("{foot}");
        }
    }
    std::process::ExitCode::SUCCESS
}

/// `models --probe [vendor/model]`: one minimal request against the
/// configured (or the given) model — the only way to learn the gateway
/// stopped serving it, since the registry still lists dead models.
fn probe(config: &std::path::Path, target: &str) -> std::process::ExitCode {
    let paths = Paths::new(Paths::default_root());
    let selector = if target.is_empty() {
        match ka_agent_core::DaemonConfig::load(config)
            .unwrap_or_default()
            .model
        {
            Some(m) => m,
            None => {
                eprintln!("ka-agent: no model configured — ka-agent model provider/model");
                return std::process::ExitCode::FAILURE;
            }
        }
    } else {
        target.to_string()
    };
    let sel = match catalog::validate_selector(&selector) {
        Ok(sel) => sel,
        Err(e) => {
            eprintln!("ka-agent: {e}");
            return std::process::ExitCode::FAILURE;
        }
    };
    let Some(p) = catalog::provider(&sel.vendor) else {
        eprintln!("ka-agent: unknown provider {}", sel.vendor);
        return std::process::ExitCode::FAILURE;
    };
    let env = catalog::read_env_file(&catalog::engine_env_path(&paths.engine_home()));
    let key = sel
        .key_env
        .as_deref()
        .and_then(|e| env.get(e).cloned().or_else(|| std::env::var(e).ok()));
    let Ok(rt) = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    else {
        eprintln!("ka-agent: cannot build async runtime");
        return std::process::ExitCode::FAILURE;
    };
    let model = sel.id.split('@').next().unwrap_or(&sel.id).to_string();
    let outcome = rt.block_on(crate::models_live::probe(&p, &model, key.as_deref()));
    println!("{} {}", outcome.mark(), outcome.line(&model));
    std::process::ExitCode::SUCCESS
}
