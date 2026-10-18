//! The resident daemon: `ka-agent run` (runtime + channels + admin socket
//! + optional web UI) and the local admin clients (`status`, `restart`).

use std::path::PathBuf;
use std::sync::Arc;

use ka_agent_core::{ApprovalStatus, DaemonConfig, SessionStatus};
use ka_agent_runtime::surface::ApprovalCard;
use ka_agent_runtime::{Runtime, Surface, paths::Paths};
use ka_agent_store::Store;

use crate::identity::migrate_config_identities;

/// The no-channel surface: local observability only. Telegram, when enabled,
/// is the real surface; this keeps a daemon without channels from being mute.
struct NullSurface;

impl Surface for NullSurface {
    fn on_note(&self, text: &str) {
        println!("· {text}");
    }

    fn on_approval(&self, card: &ApprovalCard) {
        println!(
            "┌ approval {} (expires in {})\n│ {}",
            card.approval_id,
            ka_agent_channels::commands::fmt_interval(card.expires_at_ms - ka_agent_core::now_ms()),
            card.question
        );
    }

    fn on_approval_settled(&self, approval_id: &str, status: ApprovalStatus) {
        println!("· approval {approval_id} → {status:?}");
    }

    fn on_turn_finished(&self, stop: &str, cost_usd: f64, tokens: u64) {
        println!("· turn {stop} · ${cost_usd:.4} · {tokens} tokens");
    }

    fn on_session_status(&self, status: SessionStatus) {
        println!("· session {status:?}");
    }

    fn on_error(&self, class: &str, message: &str) {
        println!("! [{class}] {message}");
    }
}

/// Current process uid without libc: `/proc/self` is a symlink to
/// `/proc/<pid>`, owned by the process's real uid.
fn current_uid() -> u32 {
    use std::os::unix::fs::MetadataExt;
    std::fs::metadata("/proc/self")
        .map(|m| m.uid())
        .unwrap_or(u32::MAX)
}

pub async fn run_daemon(
    config: PathBuf,
    scenario: Option<PathBuf>,
    web_port: Option<u16>,
) -> std::process::ExitCode {
    // Signals first: from the first line of startup, SIGINT/SIGTERM must be
    // graceful — a kill arriving during init would otherwise terminate the
    // process by default (verified by the matrix e2e restart race).
    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .map_err(|e| eprintln!("ka-agent: cannot watch SIGTERM: {e}"))
        .ok();
    let ctrl_c = tokio::signal::ctrl_c();

    // Strict, like every long-running path: a daemon with a broken config
    // must not silently run with defaults.
    let cfg = match DaemonConfig::load(&config) {
        Ok(c) => c,
        Err(e) => {
            eprintln!(
                "ka-agent: config ({}) invalid: {e}\nrun `ka-agent init` to create one",
                config.display()
            );
            return std::process::ExitCode::FAILURE;
        }
    };
    let paths = Paths::new(Paths::default_root());
    if let Err(e) = paths.ensure_dirs() {
        eprintln!("ka-agent: cannot create {}: {e}", paths.root().display());
        return std::process::ExitCode::FAILURE;
    }
    let store = match Store::open(&paths.state_db()) {
        Ok(s) => Arc::new(s),
        Err(e) => {
            eprintln!("ka-agent: cannot open {}: {e}", paths.state_db().display());
            return std::process::ExitCode::FAILURE;
        }
    };
    // The deprecated config allowlist migrates into the identities table
    // (§8.1): pairing is CLI-only now.
    let migrated = migrate_config_identities(&store, &cfg);
    if migrated > 0 {
        println!(
            "migrated {migrated} telegram.allow_user_ids entr(ies) into identities \
             — remove the key from ka-agent.toml; manage links with `ka-agent identity`"
        );
    }

    // Telegram token, resolved once (surface and channel share it).
    let token = if cfg.telegram.enabled {
        let secret_name = cfg
            .telegram
            .token_secret
            .clone()
            .unwrap_or_else(|| "telegram.bot_token".to_string());
        match ka_agent_channels::secret::resolve(&secret_name, &paths.root().join("secrets")) {
            Ok(t) => Some(t),
            Err(e) => {
                eprintln!(
                    "ka-agent: cannot resolve telegram token {secret_name:?}: {e}\n\
                     set KA_AGENT_SECRET_TELEGRAM_BOT_TOKEN or run: \
                     ka-agent secret set {secret_name}"
                );
                return std::process::ExitCode::FAILURE;
            }
        }
    } else {
        None
    };
    // Matrix token (resolved once; the surface and channel share it).
    let matrix_token = if cfg.matrix.enabled {
        let secret_name = cfg
            .matrix
            .token_secret
            .clone()
            .unwrap_or_else(|| "matrix.access_token".to_string());
        match ka_agent_channels::secret::resolve(&secret_name, &paths.root().join("secrets")) {
            Ok(t) => Some(t),
            Err(e) => {
                eprintln!(
                    "ka-agent: cannot resolve matrix token {secret_name:?}: {e}\n\
                     set KA_AGENT_SECRET_MATRIX_ACCESS_TOKEN or run: \
                     ka-agent secret set {secret_name}"
                );
                return std::process::ExitCode::FAILURE;
            }
        }
    } else {
        None
    };
    // Surfaces fan out to every enabled channel (Phase 6); a channel-less
    // daemon keeps the local NullSurface so it stays observable.
    let mut surfaces: Vec<Arc<dyn Surface>> = Vec::new();
    let _telegram_surface = token.as_ref().map(|t| {
        let s: Arc<ka_agent_channels::TelegramSurface> = Arc::new(
            ka_agent_channels::TelegramSurface::new(cfg.telegram.api_base().to_string(), t.clone()),
        );
        surfaces.push(s.clone());
        s
    });
    let matrix_surface = matrix_token.as_ref().map(|t| {
        let homeserver = cfg.matrix.homeserver.clone().unwrap_or_default();
        let s: Arc<ka_agent_channels::MatrixSurface> = Arc::new(
            ka_agent_channels::MatrixSurface::new(ka_agent_channels::MatrixConfig {
                homeserver,
                user_id: cfg.matrix.user_id.clone(),
                token: t.clone(),
            }),
        );
        surfaces.push(s.clone());
        s
    });
    let surface: Arc<dyn Surface> = if surfaces.is_empty() {
        Arc::new(NullSurface)
    } else {
        Arc::new(ka_agent_channels::MultiSurface::new(surfaces))
    };

    let mut port = ka_agent_port::WorkerPort::with_current_exe();
    if let Some(sc) = scenario {
        port.scenario_override = Some(sc);
    }
    let runtime = match Runtime::new(
        cfg.clone(),
        paths.clone(),
        Arc::clone(&store),
        port,
        Arc::clone(&surface),
    ) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("ka-agent: runtime init failed: {e}");
            return std::process::ExitCode::FAILURE;
        }
    };
    // No-agent cron handlers (M11): the calendar digest runs without the
    // engine. Registered right after the runtime exists, before any tick.
    {
        let st = Arc::clone(&store);
        runtime.set_no_agent_handler(
            "calendar",
            Arc::new(move |src| ka_agent_channels::calendar::digest(src, &st)),
        );
    }
    println!(
        "ka-agent run · session {} · data {}",
        runtime.session_id(),
        paths.root().display()
    );

    let maintenance = runtime.clone();
    tokio::spawn(async move {
        maintenance
            .run_maintenance(std::time::Duration::from_secs(15))
            .await;
    });

    let (shutdown_tx, mut shutdown_rx) = tokio::sync::watch::channel(false);

    // Channels (inbound) when enabled: one task each, same runtime.
    if let Some(token) = token {
        let tcfg = ka_agent_channels::TelegramConfig::from_config(&cfg, token);
        let channel =
            ka_agent_channels::TelegramChannel::new(tcfg, runtime.clone(), Arc::clone(&store));
        tokio::spawn(channel.run(shutdown_rx.clone()));
    }
    if let (Some(surface), Some(token)) = (matrix_surface, matrix_token) {
        let mcfg = ka_agent_channels::MatrixConfig {
            homeserver: cfg.matrix.homeserver.clone().unwrap_or_default(),
            user_id: cfg.matrix.user_id.clone(),
            token,
        };
        let channel = ka_agent_channels::MatrixChannel::new(
            mcfg,
            runtime.clone(),
            Arc::clone(&store),
            surface,
        );
        use ka_agent_channels::Channel as _;
        tokio::spawn(channel.run(shutdown_rx.clone()));
        println!(
            "matrix channel on {}",
            cfg.matrix.homeserver.clone().unwrap_or_default()
        );
    }

    // Admin socket: one newline-delimited JSON request per connection.
    let sock_path = paths.root().join("admin.sock");
    let _ = std::fs::remove_file(&sock_path);
    let listener = match tokio::net::UnixListener::bind(&sock_path) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("ka-agent: cannot bind {}: {e}", sock_path.display());
            return std::process::ExitCode::FAILURE;
        }
    };
    {
        use std::os::unix::fs::PermissionsExt;
        if let Err(e) = std::fs::set_permissions(&sock_path, std::fs::Permissions::from_mode(0o600))
        {
            eprintln!("ka-agent: cannot chmod {}: {e}", sock_path.display());
            return std::process::ExitCode::FAILURE;
        }
    }
    println!("admin socket {}", sock_path.display());

    // Web UI (M7): loopback-only, session auth; enabled with --web-port.
    if let Some(web_port) = web_port {
        let web = ka_agent_web::WebServer::new(web_port, runtime.clone(), Arc::clone(&store));
        let web_rx = shutdown_rx.clone();
        tokio::spawn(async move {
            if let Err(e) = web.run(web_rx).await {
                eprintln!("ka-agent: web server: {e}");
            }
        });
        println!(
            "web UI http://127.0.0.1:{web_port} (login: ka-agent web-token --port {web_port})"
        );
    }

    let admin_runtime = runtime.clone();
    let admin_store = Arc::clone(&store);
    let admin_shutdown = shutdown_tx.clone();
    let admin = tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                continue;
            };
            eprintln!("ka-agent: admin connection");
            let rt = admin_runtime.clone();
            let st = Arc::clone(&admin_store);
            let tx = admin_shutdown.clone();
            tokio::spawn(async move {
                if let Err(e) = serve_admin(stream, rt, st, tx).await {
                    eprintln!("ka-agent: admin request: {e}");
                }
            });
        }
    });

    // Graceful shutdown: SIGINT, SIGTERM, or the admin socket's `shutdown`
    // op (which is what `ka-agent restart` drives). The streams were
    // registered before any channel started.
    tokio::select! {
        _ = ctrl_c => {}
        _ = async {
            match term.as_mut() {
                Some(s) => {
                    let _ = s.recv().await;
                }
                None => std::future::pending::<()>().await,
            }
        } => {}
        _ = shutdown_rx.changed() => {}
    }
    eprintln!("ka-agent: shutting down");
    let _ = shutdown_tx.send(true);
    admin.abort();
    // The admin socket is created (and removed) at startup normally; leaving
    // it behind on exit made `ka-agent restore` refuse after any clean stop
    // ("daemon appears to be running") — remove it here so a stopped daemon
    // looks stopped.
    let _ = std::fs::remove_file(paths.root().join("admin.sock"));
    // Give the Telegram channel a moment to finish its in-flight poll.
    tokio::time::sleep(std::time::Duration::from_secs(3)).await;
    std::process::ExitCode::SUCCESS
}

async fn serve_admin(
    mut stream: tokio::net::UnixStream,
    runtime: Runtime,
    store: Arc<Store>,
    shutdown_tx: tokio::sync::watch::Sender<bool>,
) -> std::io::Result<()> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let peer_uid = stream.peer_cred().map(|c| c.uid()).unwrap_or(u32::MAX);
    // One bounded line: a request bigger than this is not one of ours, and
    // an unbounded read hands any local peer an unbounded buffer.
    const MAX_REQUEST: usize = 8 * 1024;
    let mut line = Vec::new();
    let mut buf = [0u8; 1024];
    loop {
        let n = stream.read(&mut buf).await?;
        if n == 0 {
            return Ok(());
        }
        line.extend_from_slice(&buf[..n]);
        if line.len() > MAX_REQUEST {
            return Ok(());
        }
        if line.contains(&b'\n') {
            break;
        }
    }
    if line.is_empty() {
        return Ok(());
    }
    let reply: serde_json::Value = if peer_uid != current_uid() {
        serde_json::json!({"error": "forbidden"})
    } else {
        match std::str::from_utf8(&line)
            .ok()
            .and_then(|s| serde_json::from_str::<serde_json::Value>(s.trim()).ok())
        {
            Some(req) if req.get("op").and_then(|o| o.as_str()) == Some("status") => {
                let (status, strand, usd, tokens, crashes) = runtime.status();
                let queue = store.queue_len(&runtime.session_id()).unwrap_or(0);
                serde_json::json!({
                    "ok": true,
                    "session": runtime.session_id().to_string(),
                    "status": format!("{status:?}"),
                    "strand": strand,
                    "budget_usd": usd,
                    "budget_tokens": tokens,
                    "crashes": crashes,
                    "queue": queue,
                })
            }
            // Graceful stop (CLI-only policy, §5): the process exits 0 and
            // the supervisor (systemd unit) brings it back — this is what
            // `ka-agent restart` drives.
            Some(req) if req.get("op").and_then(|o| o.as_str()) == Some("shutdown") => {
                let _ = shutdown_tx.send(true);
                serde_json::json!({"ok": true, "shutting_down": true})
            }
            Some(_) => serde_json::json!({"error": "unknown op"}),
            None => serde_json::json!({"error": "bad json"}),
        }
    };
    let (r, mut w) = stream.into_split();
    drop(r);
    w.write_all(format!("{reply}\n").as_bytes()).await?;
    w.flush().await
}

/// `ka-agent restart`: graceful stop over the admin socket. Without a
/// supervisor the daemon simply stops; with the systemd unit
/// (`Restart=always`) it comes straight back — that pairing is the restart.
pub fn restart_client() -> std::process::ExitCode {
    let sock = Paths::new(Paths::default_root()).root().join("admin.sock");
    let mut stream = match std::os::unix::net::UnixStream::connect(&sock) {
        Ok(s) => s,
        Err(e) => {
            eprintln!(
                "ka-agent: cannot connect {}: {e} (is the daemon running?)",
                sock.display()
            );
            return std::process::ExitCode::FAILURE;
        }
    };
    use std::io::{BufRead, BufReader, Write};
    if let Err(e) = stream.write_all(b"{\"op\":\"shutdown\"}\n") {
        eprintln!("ka-agent: send: {e}");
        return std::process::ExitCode::FAILURE;
    }
    let _ = stream.flush();
    let reply: Option<String> = BufReader::new(stream).lines().next().and_then(|l| l.ok());
    match reply {
        Some(line) => println!("{line}"),
        None => println!("\"no reply\""),
    }
    println!("daemon stopping (systemd restarts it if the unit is installed)");
    std::process::ExitCode::SUCCESS
}

pub fn status_client() -> std::process::ExitCode {
    // The socket path depends only on the data root; no config is needed.
    let sock = Paths::new(Paths::default_root()).root().join("admin.sock");
    if !sock.exists() {
        eprintln!(
            "ka-agent: no admin socket at {}\nis the daemon running? start it with `ka-agent run`",
            sock.display()
        );
        return std::process::ExitCode::FAILURE;
    }
    let mut stream = match std::os::unix::net::UnixStream::connect(&sock) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("ka-agent: cannot connect {}: {e}", sock.display());
            return std::process::ExitCode::FAILURE;
        }
    };
    let _ = stream.set_read_timeout(Some(std::time::Duration::from_secs(3)));
    let _ = stream.set_write_timeout(Some(std::time::Duration::from_secs(3)));
    use std::io::{BufRead, BufReader, Write};
    if let Err(e) = stream.write_all(b"{\"op\":\"status\"}\n") {
        eprintln!("ka-agent: send: {e}");
        return std::process::ExitCode::FAILURE;
    }
    let _ = stream.flush();
    let mut line = String::new();
    match BufReader::new(stream).read_line(&mut line) {
        Ok(0) | Err(_) => {
            eprintln!("ka-agent: daemon closed the connection without a reply");
            return std::process::ExitCode::FAILURE;
        }
        Ok(_) => {}
    }
    let v: serde_json::Value = match serde_json::from_str(line.trim()) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("ka-agent: bad reply: {e}");
            return std::process::ExitCode::FAILURE;
        }
    };
    if v.get("error").is_some() {
        eprintln!("ka-agent: {}", v["error"].as_str().unwrap_or("error"));
        return std::process::ExitCode::FAILURE;
    }
    let strand = v["strand"].as_str().unwrap_or("(new)");
    println!(
        "session {} · {}",
        v["session"].as_str().unwrap_or("?"),
        v["status"].as_str().unwrap_or("?")
    );
    println!("strand: {strand}");
    println!(
        "budget: ${:.4} + {} tokens",
        v["budget_usd"].as_f64().unwrap_or(0.0),
        v["budget_tokens"].as_u64().unwrap_or(0)
    );
    println!("queue: {}", v["queue"].as_u64().unwrap_or(0));
    println!("crashes: {}", v["crashes"].as_u64().unwrap_or(0));
    std::process::ExitCode::SUCCESS
}
