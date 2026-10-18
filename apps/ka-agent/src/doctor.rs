//! `ka-agent doctor`: the instance health report. Read-only; every check
//! names what it looked at and what to do about it.

use std::path::PathBuf;

use ka_agent_core::DaemonConfig;
use ka_agent_runtime::paths::Paths;
use ka_agent_store::Store;

/// sha256 of the shipped contract suite (docs/contract/*.md, sorted by
/// name, embedded at build time): the behavioral contract this binary was
/// built against. Support compares this before reasoning about behavior.
fn contract_suite_hash() -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    for (name, bytes) in CONTRACT_DOCS {
        hasher.update(name.as_bytes());
        hasher.update(b"\0");
        hasher.update(bytes);
        hasher.update(b"\0");
    }
    let digest = hasher.finalize();
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

/// The contract documents, embedded: (file name, bytes), sorted by name.
static CONTRACT_DOCS: &[(&str, &[u8])] = &[
    (
        "admin-socket.md",
        include_bytes!("../../../docs/contract/admin-socket.md"),
    ),
    (
        "cron-and-refs.md",
        include_bytes!("../../../docs/contract/cron-and-refs.md"),
    ),
    (
        "grants.md",
        include_bytes!("../../../docs/contract/grants.md"),
    ),
    (
        "ka-port.md",
        include_bytes!("../../../docs/contract/ka-port.md"),
    ),
    (
        "matrix.md",
        include_bytes!("../../../docs/contract/matrix.md"),
    ),
    (
        "mcp-bridge.md",
        include_bytes!("../../../docs/contract/mcp-bridge.md"),
    ),
    (
        "memory.md",
        include_bytes!("../../../docs/contract/memory.md"),
    ),
    ("ops.md", include_bytes!("../../../docs/contract/ops.md")),
    (
        "plugins.md",
        include_bytes!("../../../docs/contract/plugins.md"),
    ),
    (
        "skills.md",
        include_bytes!("../../../docs/contract/skills.md"),
    ),
    (
        "telegram.md",
        include_bytes!("../../../docs/contract/telegram.md"),
    ),
    (
        "web-ui.md",
        include_bytes!("../../../docs/contract/web-ui.md"),
    ),
    (
        "worker-protocol.md",
        include_bytes!("../../../docs/contract/worker-protocol.md"),
    ),
];

pub fn doctor(config: PathBuf) -> std::process::ExitCode {
    // doctor stays informative even with a missing/broken config: the
    // report itself is the diagnostic.
    let cfg = match DaemonConfig::load(&config) {
        Ok(c) => c,
        Err(e) => {
            if !config.exists() {
                println!(
                    "config : MISSING at {} — run `ka-agent init`",
                    config.display()
                );
            } else {
                println!("config : INVALID: {e} (continuing with defaults)");
            }
            DaemonConfig::default()
        }
    };
    let paths = Paths::new(Paths::default_root());
    println!("ka-agent {}", env!("CARGO_PKG_VERSION"));
    println!("ka pin : {} (crates.io, exact)", ka_agent_port::KA_PIN);
    let hash = contract_suite_hash();
    println!(
        "contract suite: {}… ({} documents)",
        &hash[..16],
        CONTRACT_DOCS.len()
    );
    println!(
        "config : {} (workspace {})",
        config.display(),
        cfg.workspace_path().display()
    );
    println!("data   : {}", paths.root().display());
    match paths.ensure_dirs() {
        Ok(()) => println!("dirs   : ok"),
        Err(e) => println!("dirs   : FAIL {e}"),
    }
    match Store::open(&paths.state_db()) {
        Ok(s) => {
            let v = s
                .user_version()
                .map(|v| v.to_string())
                .unwrap_or_else(|e| format!("? {e}"));
            println!("state  : {} (schema v{v})", paths.state_db().display())
        }
        Err(e) => println!("state  : FAIL {e}"),
    }
    let ws = cfg.workspace_path();
    match std::fs::create_dir_all(&ws) {
        Ok(()) => println!("workspace: {} ok", ws.display()),
        Err(e) => println!("workspace: FAIL {e}"),
    }
    if let Err(e) = cfg.validate() {
        println!("config validate: FAIL {e}");
    } else {
        println!("config validate: ok");
    }
    // Engine provider key (ka's .env tier — the file `ka-agent key set`
    // writes and the worker's engine reads).
    let engine_env = paths.engine_home().join(".config/ka/.env");
    let key_count = std::fs::read_to_string(&engine_env)
        .map(|t| {
            t.lines()
                .filter(|l| {
                    l.split_once('=')
                        .is_some_and(|(n, v)| !n.trim().is_empty() && !v.trim().is_empty())
                })
                .count()
        })
        .unwrap_or(0);
    if key_count > 0 {
        println!("provider key: {key_count} set ({})", engine_env.display());
    } else {
        println!("provider key: NONE — the engine cannot call a model until you run:");
        println!("             ka-agent key set ANTHROPIC_API_KEY <key>");
    }
    // Memory backing: stable memory is git-backed, so the git binary is a
    // hard prerequisite of the daemon's first start.
    match std::process::Command::new("git").arg("--version").output() {
        Ok(out) if out.status.success() => {
            let raw = String::from_utf8_lossy(&out.stdout).into_owned();
            let v = raw.trim().strip_prefix("git version ").unwrap_or("?");
            println!(
                "memory : git {v} backing {}",
                paths.root().join("memory").display()
            );
        }
        _ => println!("memory : FAIL git missing — the daemon cannot start (git-backed memory)"),
    }
    // Provisioned skills (SEED.md §9.1): what the engine can be asked to
    // apply with /skill.
    let skills_count = ka_agent_runtime::skills::Skills::new(&paths.engine_home())
        .enabled()
        .len();
    println!("skills : {skills_count} enabled (ka-agent skills list)");
    // Sandbox prerequisite (M8): bubblewrap, present and runnable.
    match std::process::Command::new("bwrap")
        .arg("--version")
        .output()
    {
        Ok(out) if out.status.success() => {
            let raw = String::from_utf8_lossy(&out.stdout).into_owned();
            let v = raw.trim().strip_prefix("bubblewrap ").unwrap_or("?");
            println!("sandbox: bubblewrap {v}");
        }
        _ => println!("sandbox: FAIL bubblewrap missing (plugins cannot run)"),
    }
    // Instance state a support conversation needs at a glance.
    if let Ok(store) = Store::open(&paths.state_db()) {
        let ids = store.list_identities().map(|v| v.len()).unwrap_or(0);
        println!("identities: {ids} linked");
        let lock = ka_agent_plugins::Lockfile::load(&paths.root().join("plugins/installed.json"));
        match lock {
            Ok(l) => {
                let enabled = l.plugins.iter().filter(|p| p.enabled).count();
                println!("plugins : {} installed, {enabled} enabled", l.plugins.len());
            }
            Err(e) => println!("plugins : lockfile error: {e}"),
        }
    }
    // Last backup: `backup` records a pointer at <data>/backups/latest.json.
    let pointer = paths.root().join("backups/latest.json");
    match std::fs::read_to_string(&pointer)
        .ok()
        .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok())
    {
        Some(m) => {
            let at = m["at"].as_str().unwrap_or("?");
            println!("backup : latest {at}");
        }
        None => println!("backup : none yet (run `ka-agent backup` or install the timer)"),
    }
    std::process::ExitCode::SUCCESS
}
