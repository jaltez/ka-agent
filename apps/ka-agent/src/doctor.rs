//! `ka-agent doctor`: the instance health report. Read-only (it creates
//! nothing), every check is one line: what it looked at, and the exact
//! command that fixes it when it is wrong. Exit 1 when any check failed
//! (`✗`); warnings (`!`) do not fail.

use std::path::PathBuf;

use ka_agent_core::DaemonConfig;
use ka_agent_port::catalog::{self, CredentialCheck, KeyState};
use ka_agent_runtime::paths::Paths;

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
/// A new document must be appended here AND its expected count updated in
/// `ops_e2e.rs::doctor_reports_pin_contract_state_and_backup`.
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

struct Report {
    failed: bool,
}

impl Report {
    fn new() -> Self {
        Self { failed: false }
    }

    fn ok(&self, line: String) {
        println!("✓ {line}");
    }

    fn info(&self, line: &str) {
        println!("· {line}");
    }

    fn warn(&self, line: &str) {
        println!("! {line}");
    }

    fn fail(&mut self, line: String) {
        println!("✗ {line}");
        self.failed = true;
    }
}

pub fn doctor(config: PathBuf, online: bool) -> std::process::ExitCode {
    let rep = Report::new();
    let mut rep = rep;
    let paths = Paths::new(Paths::default_root());
    println!(
        "ka-agent {} · ka {} · contracts {}… ({} documents)",
        env!("CARGO_PKG_VERSION"),
        ka_agent_port::KA_PIN,
        &contract_suite_hash()[..16],
        CONTRACT_DOCS.len()
    );

    // Config: present, parseable, valid.
    let cfg = if !config.exists() {
        rep.fail(format!(
            "no config at {} — run `ka-agent setup`",
            config.display()
        ));
        None
    } else {
        match DaemonConfig::load(&config) {
            Ok(c) => {
                rep.ok(format!("config {}", config.display()));
                Some(c)
            }
            Err(e) => {
                rep.fail(format!(
                    "config {} invalid: {e} — run `ka-agent setup` or `ka-agent config edit`",
                    config.display()
                ));
                None
            }
        }
    };

    // Model + its provider key (the whole point of setup).
    let model = cfg.as_ref().and_then(|c| c.model.clone());
    match &model {
        Some(model) => {
            rep.ok(format!("model {model}"));
            if let Ok(sel) = catalog::validate_selector(model) {
                let env = catalog::read_env_file(&catalog::engine_env_path(&paths.engine_home()));
                match catalog::key_state(&sel, &env) {
                    KeyState::Present => {
                        if let Some(env) = sel.key_env {
                            rep.ok(format!("provider key {env} set"));
                        } else {
                            rep.info("provider needs no key (local)");
                        }
                    }
                    KeyState::Missing(env) => {
                        rep.fail(format!("{env} missing — ka-agent key set {env}"))
                    }
                    KeyState::Keyless => rep.info("provider needs no key (local)"),
                }
            }
        }
        None => rep.fail("no model — run `ka-agent setup`".into()),
    }

    // git (memory backing is a git repo).
    match std::process::Command::new("git").arg("--version").output() {
        Ok(out) if out.status.success() => {
            let v = String::from_utf8_lossy(&out.stdout)
                .trim()
                .strip_prefix("git version ")
                .unwrap_or("?")
                .to_string();
            rep.ok(format!("git {v}"));
        }
        _ => rep.fail("git missing — memory needs it; install git".into()),
    }

    // Daemon.
    let daemon_up = crate::admin::is_running(&paths);
    if daemon_up {
        rep.ok("daemon running".into());
    } else {
        rep.warn("not running — ka-agent service install (or ka-agent run)");
    }

    // Service definition installed?
    if crate::service::supported() {
        if crate::service::is_installed() {
            rep.ok("background service installed".into());
            if !daemon_up && crate::service::is_active() {
                rep.warn(
                    "service is active but the daemon does not answer — check `ka-agent logs`",
                );
            }
        } else {
            rep.warn("background service not installed — ka-agent service install");
        }
    }

    // Channels: token resolvable + someone paired.
    if let Some(cfg) = &cfg {
        let secrets = paths.root().join("secrets");
        if cfg.telegram.enabled {
            let name = cfg
                .telegram
                .token_secret
                .clone()
                .unwrap_or_else(|| "telegram.bot_token".into());
            match ka_agent_channels::secret::resolve(&name, &secrets) {
                Ok(_) => rep.ok(format!("telegram token {name:?} resolves")),
                Err(e) => rep.fail(format!(
                    "telegram token {name:?} missing: {e} — ka-agent setup telegram"
                )),
            }
            if paths.state_db().exists()
                && let Ok(store) = ka_agent_store::Store::open(&paths.state_db())
            {
                let linked = store
                    .list_identities()
                    .map(|rows| rows.iter().any(|r| r.channel == "telegram"))
                    .unwrap_or(false);
                if linked {
                    rep.ok("telegram: someone paired".into());
                } else {
                    rep.warn("telegram: no one paired — ka-agent pair telegram");
                }
            }
        }
        if cfg.matrix.enabled {
            let name = cfg
                .matrix
                .token_secret
                .clone()
                .unwrap_or_else(|| "matrix.access_token".into());
            match ka_agent_channels::secret::resolve(&name, &secrets) {
                Ok(_) => rep.ok(format!("matrix token {name:?} resolves")),
                Err(e) => rep.fail(format!(
                    "matrix token {name:?} missing: {e} — ka-agent setup matrix"
                )),
            }
        }
        // Web.
        if cfg.web.enabled {
            let url_in_meta = paths
                .state_db()
                .exists()
                .then(|| ka_agent_store::Store::open(&paths.state_db()).ok())
                .flatten()
                .and_then(|s| s.meta_get("web.url").ok().flatten())
                .is_some_and(|u| !u.is_empty());
            if daemon_up && !url_in_meta {
                rep.warn("web UI not answering (port busy?) — check `ka-agent logs`");
            } else {
                rep.info(&format!(
                    "web UI port {} (ka-agent dashboard)",
                    cfg.web.port
                ));
            }
        } else {
            rep.warn("web UI disabled — ka-agent config set web.enabled true");
        }
        // Timezone.
        if cfg.timezone.is_some() {
            rep.ok(format!(
                "timezone {}",
                cfg.time_zone().iana_name().unwrap_or("UTC")
            ));
        } else {
            let sys = jiff::tz::TimeZone::system()
                .iana_name()
                .unwrap_or("UTC")
                .to_string();
            rep.warn(&format!(
                "using system zone {sys} — ka-agent config set timezone \"{sys}\""
            ));
        }
    }

    // Sandbox: plugins need bubblewrap, which is Linux-only today.
    #[cfg(target_os = "linux")]
    match std::process::Command::new("bwrap")
        .arg("--version")
        .output()
    {
        Ok(out) if out.status.success() => {
            let v = String::from_utf8_lossy(&out.stdout)
                .trim()
                .strip_prefix("bubblewrap ")
                .unwrap_or("?")
                .to_string();
            rep.ok(format!("sandbox bubblewrap {v}"));
        }
        _ => rep.warn("bubblewrap missing — plugins cannot run; install bubblewrap"),
    }
    #[cfg(not(target_os = "linux"))]
    rep.info("plugins need Linux");

    // Instance state (only when the store exists; doctor creates nothing).
    if paths.state_db().exists()
        && let Ok(store) = ka_agent_store::Store::open(&paths.state_db())
    {
        let ids = store.list_identities().map(|v| v.len()).unwrap_or(0);
        rep.ok(format!("identities {ids} linked"));
        match ka_agent_plugins::Lockfile::load(&paths.root().join("plugins/installed.json")) {
            Ok(l) => {
                let enabled = l.plugins.iter().filter(|p| p.enabled).count();
                rep.info(&format!(
                    "plugins: {} installed, {enabled} enabled",
                    l.plugins.len()
                ));
            }
            Err(_) => rep.info("plugins: none installed"),
        }
        let now = ka_agent_core::now_ms();
        // Backup age comes from the pointer file `backup` writes.
        match std::fs::read_to_string(paths.root().join("backups/latest.json"))
            .ok()
            .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok())
            .and_then(|m| m["at_ms"].as_i64().or(None))
        {
            Some(at) => {
                let ago = now - at;
                let zone = jiff::tz::TimeZone::system();
                rep.ok(format!(
                    "last backup {}",
                    ka_agent_core::when::describe(at, &jiff::Timestamp::now().to_zoned(zone))
                ));
                if ago > 7 * 86_400_000 {
                    rep.warn(
                        "last backup more than a week old — `ka-agent backup` or check the timer",
                    );
                }
            }
            None => {
                rep.warn("no backup yet — ka-agent backup (or install the service for the nightly timer)");
            }
        }
    }

    // --online: live credential + channel checks.
    if online
        && let Some(model) = &model
        && let Some(p) = catalog::provider(model.split('/').next().unwrap_or(""))
    {
        let env = catalog::read_env_file(&catalog::engine_env_path(&paths.engine_home()));
        let key = p
            .key_env
            .and_then(|e| env.get(e).cloned())
            .or_else(|| p.key_env.and_then(|e| std::env::var(e).ok()));
        let check = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| e.to_string())
            .map(|rt| rt.block_on(catalog::check_credentials(&p, key.as_deref())));
        match check {
            Ok(CredentialCheck::Ok { models }) => rep.ok(format!(
                "online: {} key works ({} models)",
                p.name,
                models.len()
            )),
            Ok(CredentialCheck::NoListing { .. }) => {
                rep.ok(format!("online: reached {}", p.name));
            }
            Ok(CredentialCheck::Rejected { status, .. }) => rep.fail(format!(
                "online: {} rejected the key (HTTP {status})",
                p.name
            )),
            Ok(CredentialCheck::Unreachable { message }) => {
                rep.warn(&format!("online: {message}"));
            }
            Err(e) => rep.warn(&format!("online checks unavailable: {e}")),
        }
    }

    if rep.failed {
        std::process::ExitCode::FAILURE
    } else {
        std::process::ExitCode::SUCCESS
    }
}
