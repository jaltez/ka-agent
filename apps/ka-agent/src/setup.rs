//! First-run setup: `ka-agent init`, engine provider keys, channel secrets.
//!
//! Everything here is local-only and fail-closed: no network, no guesses.
//! The one thing a new operator must decide (which provider, which key)
//! stays explicit — `init` writes the config, `key set` writes the one
//! secret the engine needs, and `doctor` verifies the result.

use std::path::{Path, PathBuf};

use ka_agent_runtime::paths::Paths;
use ka_agent_store::Store;

use crate::{KeyAction, SecretAction, config_path};

/// `ka-agent init`: write a starter config and prepare the data root.
pub fn init(config: Option<PathBuf>, workspace: Option<PathBuf>) -> std::process::ExitCode {
    let config = config_path(&config);
    if config.exists() {
        println!("config already exists: {}", config.display());
        println!("edit it there; `ka-agent doctor` checks the instance.");
        return std::process::ExitCode::SUCCESS;
    }
    let paths = Paths::new(Paths::default_root());
    let ws = workspace.unwrap_or_else(|| paths.root().join("workspace"));
    let body = format!(
        "# ka-agent daemon configuration (created by `ka-agent init`).\n\
         # Strict TOML: unknown keys are errors. Schema: crates/ka-agent-core/src/config.rs.\n\
         # Secrets never live in this file — `ka-agent key set` and `ka-agent secret set`.\n\n\
         # The single workspace directory every engine runs in.\n\
         workspace = \"{}\"\n\n\
         # Permission mode for new sessions: guarded (default) | accept_edits | free | plan.\n\
         mode = \"guarded\"\n\n\
         # Approval card TTL in seconds; unanswered approvals deny on expiry.\n\
         approval_ttl_secs = 900\n",
        ws.display()
    );
    if let Some(parent) = config.parent() {
        if let Err(e) = std::fs::create_dir_all(parent) {
            eprintln!("ka-agent: cannot create {}: {e}", parent.display());
            return std::process::ExitCode::FAILURE;
        }
    }
    if let Err(e) = std::fs::write(&config, body) {
        eprintln!("ka-agent: cannot write {}: {e}", config.display());
        return std::process::ExitCode::FAILURE;
    }
    if let Err(e) = paths.ensure_dirs() {
        eprintln!("ka-agent: cannot create {}: {e}", paths.root().display());
        return std::process::ExitCode::FAILURE;
    }
    // Open the state store and the memory tree now so the first `run` or
    // `chat` starts from a complete instance, not a half-initialized one.
    if let Err(e) = Store::open(&paths.state_db()) {
        eprintln!("ka-agent: cannot open {}: {e}", paths.state_db().display());
        return std::process::ExitCode::FAILURE;
    }
    match ka_agent_memory::MemoryStore::open(&paths.root().join("memory"))
        .and_then(|m| {
            m.install_compiled(
                &paths.engine_home().join(".config/ka"),
                ka_agent_memory::DEFAULT_BUDGET_BYTES,
            )
            .map(|_| m)
        })
        .and_then(|m| m.commit("memory: instance initialized"))
    {
        Ok(_) => {}
        Err(e) => {
            eprintln!("ka-agent: memory init: {e}");
            return std::process::ExitCode::FAILURE;
        }
    }
    println!("config   : {} (created)", config.display());
    println!("data root: {}", paths.root().display());
    println!("workspace: {}", ws.display());
    println!();
    println!("next:");
    println!("  ka-agent key set ANTHROPIC_API_KEY <key>   # the engine's provider key");
    println!("  ka-agent chat                              # talk to it in the terminal");
    println!("  ka-agent run                               # resident daemon (+ channels)");
    println!("  ka-agent doctor                            # health report");
    std::process::ExitCode::SUCCESS
}

/// The engine's dotenv tier: the worker's `XDG_CONFIG_HOME` points at
/// `<data>/engine-home/.config`, and ka resolves provider keys from
/// `<…>/ka/.env` — the exact file ka's own key prompt writes.
fn engine_env_path(paths: &Paths) -> PathBuf {
    paths.engine_home().join(".config/ka/.env")
}

/// Upsert one `NAME=value` line (ka's `save_api_key` semantics: replace the
/// existing line, append otherwise, 0600, single trailing newline).
fn upsert_env_line(path: &Path, name: &str, value: &str) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let pattern = format!("{name}=");
    let mut lines: Vec<String> = std::fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .map(str::to_string)
        .collect();
    match lines
        .iter_mut()
        .find(|l| l.trim_start().starts_with(&pattern))
    {
        Some(slot) => *slot = format!("{name}={value}"),
        None => lines.push(format!("{name}={value}")),
    }
    let mut text = lines.join("\n");
    text.push('\n');
    std::fs::write(path, text)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    }
    Ok(())
}

/// Read a secret value: from the argument, or one trimmed line from stdin
/// (`printf %s "$KEY" | ka-agent key set NAME`) so it never lands in shell
/// history.
fn value_from(arg: Option<&str>) -> Result<String, String> {
    if let Some(v) = arg {
        let v = v.trim();
        if !v.is_empty() {
            return Ok(v.to_string());
        }
    }
    let mut line = String::new();
    match std::io::BufRead::read_line(&mut std::io::stdin().lock(), &mut line) {
        Ok(0) | Err(_) => Err("no value given (pass it as an argument or on stdin)".into()),
        Ok(_) => {
            let v = line.trim();
            if v.is_empty() {
                Err("value is empty".into())
            } else {
                Ok(v.to_string())
            }
        }
    }
}

fn valid_secret_name(s: &str) -> bool {
    !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-' || c == '_')
}

/// `ka-agent key …`: the engine provider keys (ka's `.env` tier).
pub fn key_action(action: KeyAction) -> std::process::ExitCode {
    let paths = Paths::new(Paths::default_root());
    let env = engine_env_path(&paths);
    match action {
        KeyAction::Set { name, value } => {
            let name = name.trim().to_ascii_uppercase();
            if !valid_secret_name(&name) {
                eprintln!("ka-agent: key name {name:?}: [A-Za-z0-9._-] only");
                return std::process::ExitCode::FAILURE;
            }
            let value = match value_from(value.as_deref()) {
                Ok(v) => v,
                Err(e) => {
                    eprintln!("ka-agent: {e}");
                    return std::process::ExitCode::FAILURE;
                }
            };
            if let Err(e) = upsert_env_line(&env, &name, &value) {
                eprintln!("ka-agent: {}: {e}", env.display());
                return std::process::ExitCode::FAILURE;
            }
            println!("{} set in {} (0600)", name, env.display());
            println!("the engine reads it on its next start");
            std::process::ExitCode::SUCCESS
        }
        KeyAction::List => match std::fs::read_to_string(&env) {
            Ok(text) => {
                let names: Vec<&str> = text
                    .lines()
                    .filter_map(|l| l.split_once('=').map(|(n, _)| n.trim()))
                    .filter(|n| !n.is_empty())
                    .collect();
                if names.is_empty() {
                    println!("no provider keys set (ka-agent key set <NAME> <value>)");
                } else {
                    for n in names {
                        println!("{n}");
                    }
                    println!("(values are never printed; file: {})", env.display());
                }
                std::process::ExitCode::SUCCESS
            }
            Err(_) => {
                println!("no provider keys set (ka-agent key set <NAME> <value>)");
                std::process::ExitCode::SUCCESS
            }
        },
    }
}

/// `ka-agent secret …`: channel and connector tokens (the 0600 file tier
/// under `<data>/secrets`, resolved by name from the config).
pub fn secret_action(action: SecretAction) -> std::process::ExitCode {
    let paths = Paths::new(Paths::default_root());
    let dir = paths.root().join("secrets");
    match action {
        SecretAction::Set { name, value } => {
            if !valid_secret_name(&name) {
                eprintln!("ka-agent: secret name {name:?}: [a-z0-9._-] only");
                return std::process::ExitCode::FAILURE;
            }
            let value = match value_from(value.as_deref()) {
                Ok(v) => v,
                Err(e) => {
                    eprintln!("ka-agent: {e}");
                    return std::process::ExitCode::FAILURE;
                }
            };
            match ka_agent_channels::secret::store(&name, &value, &dir) {
                Ok(path) => {
                    println!("{name} set at {} (0600)", path.display());
                    std::process::ExitCode::SUCCESS
                }
                Err(e) => {
                    eprintln!("ka-agent: {e}");
                    std::process::ExitCode::FAILURE
                }
            }
        }
        SecretAction::List => match std::fs::read_dir(&dir) {
            Ok(entries) => {
                let mut names: Vec<String> = entries
                    .flatten()
                    .map(|e| e.file_name().to_string_lossy().into_owned())
                    .collect();
                names.sort();
                if names.is_empty() {
                    println!("no secrets set (ka-agent secret set <name> <value>)");
                } else {
                    for n in names {
                        println!("{n}");
                    }
                    println!("(values are never printed; dir: {})", dir.display());
                }
                std::process::ExitCode::SUCCESS
            }
            Err(_) => {
                println!("no secrets set (ka-agent secret set <name> <value>)");
                std::process::ExitCode::SUCCESS
            }
        },
    }
}
