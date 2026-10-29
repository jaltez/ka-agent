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

/// The checks as they run: printed line by line, or (`--json`) collected
/// into one document printed at the end.
struct Report {
    failed: bool,
    json: Option<std::cell::RefCell<Vec<serde_json::Value>>>,
}

impl Report {
    fn new(json: bool) -> Self {
        Self {
            failed: false,
            json: json.then(|| std::cell::RefCell::new(Vec::new())),
        }
    }

    fn emit(&self, level: &str, mark: &str, line: &str) {
        match &self.json {
            Some(rows) => rows
                .borrow_mut()
                .push(serde_json::json!({"level": level, "text": line})),
            None => println!("{mark} {line}"),
        }
    }

    fn ok(&self, line: String) {
        self.emit("ok", "✓", &line);
    }

    fn info(&self, line: &str) {
        self.emit("info", "·", line);
    }

    fn warn(&self, line: &str) {
        self.emit("warn", "!", line);
    }

    fn fail(&mut self, line: String) {
        self.emit("fail", "✗", &line);
        self.failed = true;
    }
}

/// The line for a `state.db` written by a newer binary: the store's own
/// `Newer` error otherwise vanishes inside checks that only `let Ok(...)`,
/// leaving unrelated failures on screen.
fn store_version_issue(err: &ka_agent_store::StoreError) -> Option<String> {
    match err {
        ka_agent_store::StoreError::Newer { version } => Some(format!(
            "state.db is newer than this binary (user_version {version}) — \
             run the newer ka-agent, or install the release that matches it"
        )),
        _ => None,
    }
}

/// Grants lines for the report: the full-access warning (when
/// `grants.advisory` is on) and one info line with the path grants and
/// the install state. `--json` collects them like every other check.
fn grants_lines(cfg: &DaemonConfig) -> Vec<(&'static str, String)> {
    let mut out = Vec::new();
    if cfg.grants.full && cfg.grants.advisory {
        out.push((
            "warn",
            "grants.full is on: nothing asks — the sandbox is off, installs run, path denies are ignored"
                .to_string(),
        ));
    }
    let dirs: Vec<String> = cfg
        .grants
        .path
        .iter()
        .map(|g| {
            format!(
                "{} ({})",
                g.dir.display(),
                match g.access {
                    ka_agent_core::config::Access::Allow => "allow",
                    ka_agent_core::config::Access::Ask => "ask",
                    ka_agent_core::config::Access::Deny => "deny",
                }
            )
        })
        .collect();
    let paths = if dirs.is_empty() {
        "0 paths".to_string()
    } else {
        format!("{} paths ({})", dirs.len(), dirs.join(", "))
    };
    let install = if cfg.grants.install || cfg.grants.full {
        "on"
    } else {
        "off"
    };
    out.push(("info", format!("grants: {paths}, install {install}")));
    out
}

pub fn doctor(config: PathBuf, online: bool, json: bool) -> std::process::ExitCode {
    let mut rep = Report::new(json);
    let paths = Paths::new(Paths::default_root());
    let header = format!(
        "ka-agent {} · ka {} · contracts {}… ({} documents)",
        env!("CARGO_PKG_VERSION"),
        ka_agent_port::KA_PIN,
        &contract_suite_hash()[..16],
        CONTRACT_DOCS.len()
    );
    if !json {
        println!("{header}");
    }
    if std::env::var_os("KA_AGENT_DATA_DIR").is_none() {
        rep.info(&format!(
            "profile {} · data {}",
            ka_agent_core::layout::profile_name(),
            paths.root().display()
        ));
    }

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

    // A store written by a newer binary (downgrade, or an older binary on
    // PATH): every read below fails with `database is newer …` and the
    // individual checks swallow it. Say it once, up front.
    if paths.state_db().exists()
        && let Err(e) = ka_agent_store::Store::open(&paths.state_db())
        && let Some(line) = store_version_issue(&e)
    {
        rep.fail(line);
    }

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

    // Capabilities the engine gets from Ka Agent's config.
    if let Some(cfg) = &cfg {
        let env = catalog::read_env_file(&catalog::engine_env_path(&paths.engine_home()));
        // Only the engine `.env` tier counts: the worker runs with a
        // sanitized environment and never sees this shell's variables.
        let has = |name: &str| env.get(name).is_some_and(|v| !v.is_empty());
        match &cfg.search {
            Some(s) if has(&s.key_env()) => rep.ok(format!("web search {} (key set)", s.provider)),
            Some(s) => rep.fail(format!(
                "web search {}: {} missing — ka-agent key set {}",
                s.provider,
                s.key_env(),
                s.key_env()
            )),
            None => rep.warn("web search off — ka-agent setup search"),
        }
        if let Some(v) = &cfg.voice {
            match v.key_env() {
                Some(k) if !has(k) => rep.fail(format!(
                    "voice notes {}: {k} missing — ka-agent key set {k}",
                    v.provider
                )),
                _ => rep.ok(format!("voice notes {} ({})", v.provider, v.model())),
            }
        }
        for m in &cfg.fallback {
            match catalog::validate_selector(m) {
                Ok(_) => rep.ok(format!("fallback {m}")),
                Err(e) => rep.fail(format!("fallback {m}: {e}")),
            }
        }
        let secrets_dir = paths.root().join("secrets");
        for m in &cfg.mcp {
            let missing: Vec<&str> = m
                .env
                .values()
                .chain(m.headers.values())
                .filter_map(|v| v.strip_prefix(ka_agent_port::engine_layer::SECRET_PREFIX))
                .filter(|name| ka_agent_core::secret::resolve(name, &secrets_dir).is_err())
                .collect();
            let target = m.command.as_deref().or(m.url.as_deref()).unwrap_or("?");
            if missing.is_empty() {
                rep.ok(format!("mcp {} ({target})", m.name));
            } else {
                rep.fail(format!(
                    "mcp {}: secret {} not set — ka-agent secret set {}",
                    m.name,
                    missing.join(", "),
                    missing[0]
                ));
            }
        }
        for (level, line) in grants_lines(cfg) {
            match level {
                "warn" => rep.warn(&line),
                _ => rep.info(&line),
            }
        }
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
            match ka_agent_core::secret::resolve(&name, &secrets) {
                Ok(token) => {
                    rep.ok(format!("telegram token {name:?} resolves"));
                    for p in crate::profile_cli::profiles_polling_token(&token) {
                        rep.fail(format!(
                            "profile {p} polls the same Telegram bot — two daemons on one token \
                             steal each other's updates (409); give each profile its own bot"
                        ));
                    }
                }
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
            match ka_agent_core::secret::resolve(&name, &secrets) {
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
            for p in crate::profile_cli::profiles_on_web_port(cfg.web.port) {
                rep.warn(&format!(
                    "profile {p} also uses web port {} — only one daemon can serve it; \
                     ka-agent config set web.port <n>",
                    cfg.web.port
                ));
            }
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

    // Memory: compiled under a byte budget; past it entries are left out
    // without anyone noticing unless we say so.
    if paths.root().join("memory").is_dir()
        && let Ok(mem) = ka_agent_memory::MemoryStore::open(&paths.root().join("memory"))
        && let Ok(report) = mem.compile(ka_agent_memory::DEFAULT_BUDGET_BYTES)
    {
        let line = format!(
            "memory {:.1} of {:.1} KB",
            report.total_bytes as f64 / 1024.0,
            report.budget_bytes as f64 / 1024.0
        );
        if report.truncated {
            rep.warn(&format!(
                "{line} — full: older entries are left out; prune with /memory"
            ));
        } else if report.total_bytes * 10 >= report.budget_bytes * 9 {
            rep.warn(&format!("{line} — nearly full"));
        } else {
            rep.ok(line);
        }
    }

    // A cached update check (refreshed by `ka-agent update [--check]`).
    if let Some((latest, at)) = crate::update_cli::last_check(&paths)
        && ka_agent_core::now_ms() - at < 7 * 86_400_000
    {
        let current = env!("CARGO_PKG_VERSION");
        let num = |s: &str| -> Vec<u64> {
            s.trim_start_matches('v')
                .split('.')
                .map(|p| p.parse().unwrap_or(0))
                .collect()
        };
        if num(&latest) > num(current) {
            rep.warn(&format!(
                "update available: {current} → {latest} — ka-agent update"
            ));
        }
    }

    if let Some(dir) = paths.loose_permissions() {
        rep.warn(&format!(
            "{} is readable by other users — fixed on the next start, or now: chmod 700 {}",
            dir.display(),
            dir.display()
        ));
    }

    // Stale pre-0.3 tree next to the new one (migration only runs while the
    // new base does not exist): say which one is live.
    if !std::env::var_os("KA_AGENT_HOME").is_some_and(|v| !v.is_empty())
        && !std::env::var_os("KA_AGENT_DATA_DIR").is_some_and(|v| !v.is_empty())
    {
        let legacy = ka_agent_core::layout::legacy_data_root();
        if paths.root().starts_with(ka_agent_core::layout::base_dir()) && legacy.is_dir() {
            rep.warn(&format!(
                "legacy data root {} still exists — the live one is {}; remove or merge manually",
                legacy.display(),
                paths.root().display()
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
    // Windows prerequisites (plan 4.6): the service is a Task Scheduler
    // entry and the console is driven through the Windows API.
    #[cfg(target_os = "windows")]
    match std::process::Command::new("schtasks.exe")
        .args(["/Query", "/?"])
        .output()
    {
        Ok(out) if out.status.success() => rep.ok("task scheduler available (service install)".into()),
        _ => rep.warn(
            "schtasks.exe not available — `ka-agent service install` cannot create the logon task; run `ka-agent run` instead",
        ),
    }
    #[cfg(target_os = "windows")]
    rep.info(&format!(
        "terminal chat: {} console",
        if crate::term::raw::is_tty() {
            "raw-mode"
        } else {
            "line-mode"
        }
    ));

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
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| e.to_string());
        let check = rt
            .as_ref()
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
        // The registry still lists models gateways have stopped
        // serving: one minimal request proves the configured model
        // itself answers (same wording as `ka-agent models --probe`).
        if let (Some(rt), Ok(sel)) = (rt.as_ref().ok(), catalog::validate_selector(model)) {
            let model_id = sel.id.split('@').next().unwrap_or(&sel.id).to_string();
            match rt.block_on(crate::models_live::probe(&p, &model_id, key.as_deref())) {
                crate::models_live::ProbeOutcome::Answers => {
                    rep.ok(format!("{model_id} answers"));
                }
                crate::models_live::ProbeOutcome::Refused(message) => {
                    rep.fail(format!("{model_id}: {message}"));
                }
                crate::models_live::ProbeOutcome::NotProbed => {
                    rep.info(&format!("{model_id} not probed (no key)"));
                }
            }
        }
    }

    if let Some(rows) = &rep.json {
        println!(
            "{}",
            serde_json::json!({
                "ok": !rep.failed,
                "version": env!("CARGO_PKG_VERSION"),
                "checks": rows.borrow().clone(),
            })
        );
    }
    // Human closing line only: --json must stay one document.
    if rep.json.is_none() {
        if rep.failed {
            println!("fix the ✗ lines above, then run `ka-agent doctor` again");
        } else {
            println!("all in balance.");
        }
    }
    if rep.failed {
        std::process::ExitCode::FAILURE
    } else {
        std::process::ExitCode::SUCCESS
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;
    use ka_agent_store::StoreError;

    #[test]
    fn newer_store_maps_to_a_version_and_remedy_line() {
        let line = store_version_issue(&StoreError::Newer { version: 8 }).unwrap();
        assert!(line.contains("newer than this binary"), "{line}");
        assert!(line.contains("user_version 8"), "{line}");
        assert!(line.contains("newer ka-agent"), "{line}");
        // Ordinary store errors keep their own message (no duplicate line).
        assert!(store_version_issue(&StoreError::NotFound("x".into())).is_none());
    }
    #[test]
    fn grants_lines_warn_on_full_and_list_paths() {
        let mut cfg = DaemonConfig::default();
        let lines = grants_lines(&cfg);
        assert_eq!(
            lines,
            vec![("info", "grants: 0 paths, install off".to_string())],
            "{lines:?}"
        );

        cfg.grants.full = true;
        let lines = grants_lines(&cfg);
        assert_eq!(lines[0].0, "warn", "{lines:?}");
        assert!(lines[0].1.contains("grants.full is on"), "{lines:?}");
        // full grants installs too, so the info line says on
        assert!(lines[1].1.contains("install on"), "{lines:?}");

        // advisory = false silences the warning; the info line stays.
        cfg.grants.advisory = false;
        let lines = grants_lines(&cfg);
        assert_eq!(lines.len(), 1, "{lines:?}");
        assert_eq!(lines[0].0, "info", "{lines:?}");

        cfg.grants.advisory = true;
        cfg.grants.install = true;
        cfg.grants.path = vec![
            ka_agent_core::config::PathGrant {
                dir: PathBuf::from("/srv/ok"),
                access: ka_agent_core::config::Access::Allow,
            },
            ka_agent_core::config::PathGrant {
                dir: PathBuf::from("/srv/no"),
                access: ka_agent_core::config::Access::Deny,
            },
        ];
        let text = grants_lines(&cfg)[1].1.clone();
        assert_eq!(
            text,
            "grants: 2 paths (/srv/ok (allow), /srv/no (deny)), install on"
        );
    }
}
