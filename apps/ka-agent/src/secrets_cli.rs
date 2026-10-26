//! `ka-agent key …` and `ka-agent secret …` CLI actions. Split from
//! setup.rs (moves only).

use ka_agent_port::catalog::{self, CredentialCheck};
use ka_agent_runtime::paths::Paths;

use crate::cli::{KeyAction, SecretAction};
use crate::prompt;

fn valid_secret_name(s: &str) -> bool {
    !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-' || c == '_')
}

/// Read a secret value: from the argument, or one trimmed line from stdin
/// (`printf %s "$KEY" | ka-agent key set NAME`) so it never lands in shell
/// history. On a terminal an omitted value is asked for hidden.
fn value_from(arg: Option<&str>) -> Result<String, String> {
    if let Some(v) = arg {
        let v = v.trim();
        if !v.is_empty() {
            return Ok(v.to_string());
        }
    }
    if std::io::IsTerminal::is_terminal(&std::io::stdin()) {
        return prompt::secret("Paste the value").map_err(|e| e.to_string());
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

/// `ka-agent key …`: the engine provider keys (ka's `.env` tier).
pub async fn key_action(action: KeyAction) -> std::process::ExitCode {
    let paths = Paths::new(Paths::default_root());
    let env = catalog::engine_env_path(&paths.engine_home());
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
            if let Err(e) = catalog::upsert_env_file(&env, &name, &value) {
                eprintln!("ka-agent: {}: {e}", env.display());
                return std::process::ExitCode::FAILURE;
            }
            println!("{} set in {} (0600)", name, env.display());
            // A known provider? Verify live (failures don't undo the save).
            if let Some(p) = catalog::provider_for_key_env(&name) {
                println!("checking the key with {}…", p.name);
                match catalog::check_credentials(&p, Some(&value)).await {
                    CredentialCheck::Ok { models } => {
                        println!("✓ key works ({} models available)", models.len())
                    }
                    CredentialCheck::NoListing { .. } => println!(
                        "✓ reached {} (it doesn't list models; the first message will confirm the key)",
                        p.name
                    ),
                    CredentialCheck::Rejected { status, message } => {
                        println!("✗ {} rejected the key: HTTP {status} {message}", p.name)
                    }
                    CredentialCheck::Unreachable { message } => {
                        println!("✗ couldn't reach {}: {message}", p.base_url)
                    }
                }
            }
            if crate::admin::is_running(&paths) {
                match crate::admin::request(
                    &paths,
                    &serde_json::json!({"op": "reload", "restart_worker": true}),
                ) {
                    Ok(_) => println!("the engine picks it up with your next message"),
                    Err(e) => println!("the running agent could not reload: {e}"),
                }
            } else {
                println!("saved — used when Ka Agent starts");
            }
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
            match ka_agent_core::secret::store(&name, &value, &dir) {
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
        SecretAction::Generate { name } => {
            if !valid_secret_name(&name) {
                eprintln!("ka-agent: secret name {name:?}: [a-z0-9._-] only");
                return std::process::ExitCode::FAILURE;
            }
            // 32 bytes from the OS, hex; printed once so a script can
            // capture it (the store keeps it for the daemon).
            let mut bytes = [0u8; 32];
            use rand_core::RngCore as _;
            rand_core::OsRng.fill_bytes(&mut bytes);
            let value: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
            match ka_agent_core::secret::store(&name, &value, &dir) {
                Ok(_) => {
                    println!("{value}");
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
