//! The resident daemon: `ka-agent run` (runtime + channels + admin socket
//! + web UI) and the local admin clients (`status`, `restart`).

use std::path::{Path, PathBuf};
use std::sync::Arc;

use ka_agent_core::{ApprovalStatus, DaemonConfig, SessionStatus};
use ka_agent_runtime::surface::ApprovalCard;
use ka_agent_runtime::{
    EventBus, InstanceLock, LockError, Runtime, Surface, SurfaceEvent, paths::Paths,
};
use ka_agent_store::Store;

use crate::admin;
use crate::identity::migrate_config_identities;

/// The journal surface: every runtime event as one stderr line, so the
/// service log (`ka-agent logs`) is a complete trail of what happened.
pub struct LogSurface;

impl Surface for LogSurface {
    fn on_note(&self, text: &str) {
        eprintln!("· {text}");
    }

    fn on_notice(&self, text: &str) {
        eprintln!("» {text}");
    }

    fn on_inbound(&self, source: &str, text: &str) {
        eprintln!("» you ({source}): {text}");
    }

    fn on_approval(&self, card: &ApprovalCard) {
        eprintln!(
            "┌ approval {} (expires in {})\n│ {}",
            card.approval_id,
            ka_agent_core::fmt_interval(card.expires_at_ms - ka_agent_core::now_ms()),
            card.question
        );
    }

    fn on_approval_settled(&self, approval_id: &str, status: ApprovalStatus) {
        eprintln!("· approval {approval_id} → {status:?}");
    }

    fn on_turn_finished(&self, stop: &str, cost_usd: f64, tokens: u64) {
        eprintln!("· turn {stop} · ${cost_usd:.4} · {tokens} tokens");
    }

    fn on_session_status(&self, status: SessionStatus) {
        eprintln!("· session {status:?}");
    }

    fn on_error(&self, class: &str, message: &str) {
        eprintln!("! [{class}] {message}");
    }
}

/// The exact refusal when another runtime holds the data root.
pub fn print_lock_held(root: &Path) {
    eprintln!(
        "ka-agent: another Ka Agent is already running on {} (the daemon or a local chat).",
        root.display()
    );
    eprintln!(
        "check it with `ka-agent status`; stop the daemon with `ka-agent restart` or `ka-agent service stop`"
    );
}

pub async fn run_daemon(
    config: PathBuf,
    scenario: Option<PathBuf>,
    web_port: Option<u16>,
) -> std::process::ExitCode {
    // Signals first: from the first line of startup, SIGINT/SIGTERM must be
    // graceful — a kill arriving during init would otherwise terminate the
    // process by default (verified by the matrix e2e restart race).
    // SIGTERM watching is Unix-only; Windows relies on Ctrl-C.
    #[cfg(unix)]
    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .map_err(|e| eprintln!("ka-agent: cannot watch SIGTERM: {e}"))
        .ok();
    #[cfg(not(unix))]
    let term: Option<()> = None;
    let ctrl_c = tokio::signal::ctrl_c();

    // Strict, like every long-running path: a daemon with a broken config
    // must not silently run with defaults.
    let cfg = match DaemonConfig::load(&config) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("{}", crate::config_problem(&config, &e));
            return std::process::ExitCode::FAILURE;
        }
    };
    let paths = Paths::new(Paths::default_root());
    if let Err(e) = paths.ensure_dirs() {
        eprintln!("ka-agent: cannot create {}: {e}", paths.root().display());
        return std::process::ExitCode::FAILURE;
    }
    // One runtime per data root: held for the whole function. Only after
    // acquiring it is a leftover admin socket provably stale.
    let _lock = match InstanceLock::acquire(&paths) {
        Ok(l) => l,
        Err(LockError::Held(root)) => {
            print_lock_held(&root);
            return std::process::ExitCode::FAILURE;
        }
        Err(LockError::Io(e)) => {
            eprintln!(
                "ka-agent: cannot lock {}: {e}",
                paths.instance_lock().display()
            );
            return std::process::ExitCode::FAILURE;
        }
    };
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
        match ka_agent_core::secret::resolve(&secret_name, &paths.root().join("secrets")) {
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
        match ka_agent_core::secret::resolve(&secret_name, &paths.root().join("secrets")) {
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
    // Surfaces fan out: the journal, the event bus (attached terminals and
    // the web chat), and every enabled channel.
    let bus = Arc::new(EventBus::new());
    let mut surfaces: Vec<Arc<dyn Surface>> = vec![Arc::new(LogSurface), bus.clone()];
    let _telegram_surface = token.as_ref().map(|t| {
        // The operator's last chat survives restarts, so reminders and
        // cron output arrive before anyone writes again.
        let initial_chat = store
            .meta_get("telegram.chat_id")
            .ok()
            .flatten()
            .and_then(|s| s.parse::<i64>().ok());
        let s: Arc<ka_agent_channels::TelegramSurface> =
            Arc::new(ka_agent_channels::TelegramSurface::new(
                cfg.telegram.api_base().to_string(),
                t.clone(),
                initial_chat,
            ));
        s.set_streaming(cfg.telegram.stream);
        surfaces.push(s.clone());
        s
    });
    let matrix_surface = matrix_token.as_ref().map(|t| {
        let homeserver = cfg.matrix.homeserver.clone().unwrap_or_default();
        // The last room the operator wrote from survives restarts, so
        // proactive notices are delivered before anyone writes again.
        let initial_room = store.meta_get("matrix.room_id").ok().flatten();
        let s: Arc<ka_agent_channels::MatrixSurface> =
            Arc::new(ka_agent_channels::MatrixSurface::new(
                ka_agent_channels::MatrixConfig {
                    homeserver,
                    user_id: cfg.matrix.user_id.clone(),
                    token: t.clone(),
                    transcriber: None,
                },
                initial_room,
            ));
        surfaces.push(s.clone());
        s
    });
    let surface: Arc<dyn Surface> = Arc::new(ka_agent_channels::MultiSurface::new(surfaces));

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
    runtime.set_config_path(config.clone());
    // No-agent cron handlers (M11): the calendar digest runs without the
    // engine. Registered right after the runtime exists, before any tick.
    {
        let st = Arc::clone(&store);
        let tz = cfg.time_zone();
        runtime.set_no_agent_handler(
            "calendar",
            Arc::new(move |src| ka_agent_channels::calendar::digest_in(src, &st, &tz)),
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

    // Voice-note transcription for the phone channels (key from the engine
    // `.env` tier, like every provider key).
    let transcriber = cfg.voice.as_ref().and_then(|voice| {
        let env = ka_agent_port::catalog::read_env_file(&ka_agent_port::catalog::engine_env_path(
            &paths.engine_home(),
        ));
        ka_agent_channels::voice::Transcriber::from_config(voice, &env)
            .map_err(|e| eprintln!("ka-agent: {e} (voice notes off)"))
            .ok()
    });

    // Channels (inbound) when enabled: one task each, same runtime.
    if let Some(token) = token {
        let mut tcfg = ka_agent_channels::TelegramConfig::from_config(&cfg, token);
        tcfg.transcriber = transcriber.clone();
        let channel =
            ka_agent_channels::TelegramChannel::new(tcfg, runtime.clone(), Arc::clone(&store));
        tokio::spawn(channel.run(shutdown_rx.clone()));
    }
    if let (Some(surface), Some(token)) = (matrix_surface, matrix_token) {
        let mcfg = ka_agent_channels::MatrixConfig {
            homeserver: cfg.matrix.homeserver.clone().unwrap_or_default(),
            user_id: cfg.matrix.user_id.clone(),
            token,
            transcriber: transcriber.clone(),
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

    // Admin socket: newline-delimited JSON. The lock above proves a leftover
    // socket is stale.
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
    // Peers must be the socket's owner: the uid comes from the file we just
    // created (portable; `/proc` does not exist on macOS).
    #[cfg(unix)]
    let own_uid = {
        use std::os::unix::fs::MetadataExt;
        match std::fs::metadata(&sock_path) {
            Ok(m) => m.uid(),
            Err(e) => {
                eprintln!("ka-agent: cannot stat {}: {e}", sock_path.display());
                return std::process::ExitCode::FAILURE;
            }
        }
    };
    #[cfg(not(unix))]
    let own_uid = 0u32;
    println!("admin socket {}", sock_path.display());

    // Web UI (M7): loopback-only, session auth; `[web]` in the config, or
    // `--web-port` as an override.
    let web_port = web_port.or_else(|| cfg.web.enabled.then_some(cfg.web.port));
    if let Some(web_port) = web_port {
        let web = ka_agent_web::WebServer::new(
            web_port,
            runtime.clone(),
            Arc::clone(&store),
            Arc::clone(&bus),
        );
        let web_rx = shutdown_rx.clone();
        tokio::spawn(async move {
            if let Err(e) = web.run(web_rx).await {
                eprintln!("ka-agent: web UI disabled: {e}");
            }
        });
        println!("web UI http://127.0.0.1:{web_port} (open it with: ka-agent dashboard)");
    }

    let ctx = AdminCtx {
        runtime: runtime.clone(),
        store: Arc::clone(&store),
        bus: Arc::clone(&bus),
        shutdown: shutdown_tx.clone(),
        own_uid,
        config: config.clone(),
    };
    let admin = tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                continue;
            };
            let ctx = ctx.clone();
            tokio::spawn(async move {
                if let Err(e) = serve_admin(stream, ctx).await {
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

/// Everything an admin connection needs.
#[derive(Clone)]
struct AdminCtx {
    runtime: Runtime,
    store: Arc<Store>,
    bus: Arc<EventBus>,
    shutdown: tokio::sync::watch::Sender<bool>,
    /// Owner of the socket file; only this uid may talk to the daemon.
    own_uid: u32,
    /// The config file the daemon loaded (re-read by `reload`).
    config: PathBuf,
}

/// First request line ceiling: anything bigger is not one of ours, and an
/// unbounded read hands any local peer an unbounded buffer.
const MAX_REQUEST: u64 = 8 * 1024;
/// Per-line ceiling on an attach stream (a pasted message fits).
const MAX_ATTACH_LINE: u64 = 64 * 1024;

/// Read one `\n`-terminated line of at most `max` bytes. `Ok(None)` on EOF
/// or an over-long line (the caller closes the connection).
async fn read_bounded_line<R>(r: &mut R, max: u64) -> std::io::Result<Option<String>>
where
    R: tokio::io::AsyncBufRead + Unpin,
{
    use tokio::io::{AsyncBufReadExt, AsyncReadExt};
    let mut buf = Vec::new();
    let n = (&mut *r).take(max + 1).read_until(b'\n', &mut buf).await?;
    if n == 0 || (buf.len() as u64) > max || !buf.ends_with(b"\n") {
        return Ok(None);
    }
    Ok(Some(String::from_utf8_lossy(&buf).trim().to_string()))
}

async fn serve_admin(stream: tokio::net::UnixStream, ctx: AdminCtx) -> std::io::Result<()> {
    use tokio::io::AsyncWriteExt;
    let peer_uid = stream.peer_cred().map(|c| c.uid()).unwrap_or(u32::MAX);
    let (r, mut w) = stream.into_split();
    let mut reader = tokio::io::BufReader::new(r);
    let Some(line) = read_bounded_line(&mut reader, MAX_REQUEST).await? else {
        return Ok(());
    };
    if line.is_empty() {
        return Ok(());
    }
    let reply: serde_json::Value = if peer_uid != ctx.own_uid {
        serde_json::json!({"error": "forbidden"})
    } else {
        let req = serde_json::from_str::<serde_json::Value>(&line).ok();
        let op = req
            .as_ref()
            .and_then(|r| r.get("op"))
            .and_then(|o| o.as_str())
            .map(str::to_string);
        match (req, op.as_deref()) {
            (Some(_), Some("status")) => match ctx.runtime.status_report() {
                Ok(report) => serde_json::json!({"ok": true, "report": report}),
                Err(e) => serde_json::json!({"error": e.to_string()}),
            },
            // Graceful stop (CLI-only policy, §5): the process exits 0 and
            // the service manager brings it back — this is what
            // `ka-agent restart` drives.
            (Some(_), Some("shutdown")) => {
                let _ = ctx.shutdown.send(true);
                serde_json::json!({"ok": true, "shutting_down": true})
            }
            (Some(req), Some("reload")) => {
                let restart_worker = req
                    .get("restart_worker")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                match DaemonConfig::load(&ctx.config) {
                    Ok(cfg) => {
                        let report = ctx.runtime.reload_config(cfg, restart_worker).await;
                        serde_json::json!({"ok": true, "report": report})
                    }
                    Err(e) => serde_json::json!({"error": e.to_string()}),
                }
            }
            (Some(_), Some("attach")) => return serve_attach(reader, w, ctx).await,
            (Some(_), _) => serde_json::json!({"error": "unknown op"}),
            (None, _) => serde_json::json!({"error": "bad json"}),
        }
    };
    w.write_all(format!("{reply}\n").as_bytes()).await?;
    w.flush().await
}

/// `{"op":"attach"}`: a live terminal on the daemon's session. The server
/// sends `hello` then every bus event; the client sends `say` lines that go
/// through the shared command router exactly like a channel message.
async fn serve_attach(
    mut reader: tokio::io::BufReader<tokio::net::unix::OwnedReadHalf>,
    mut w: tokio::net::unix::OwnedWriteHalf,
    ctx: AdminCtx,
) -> std::io::Result<()> {
    use tokio::io::AsyncWriteExt;
    use tokio::sync::broadcast::error::RecvError;
    // Subscribe before the hello so nothing between them is lost.
    let mut sub = ctx.bus.subscribe();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<String>();
    let writer = tokio::spawn(async move {
        while let Some(line) = rx.recv().await {
            if w.write_all(line.as_bytes()).await.is_err() || w.flush().await.is_err() {
                break;
            }
        }
    });
    let send = |tx: &tokio::sync::mpsc::UnboundedSender<String>, ev: &SurfaceEvent| -> bool {
        match serde_json::to_string(ev) {
            Ok(json) => tx.send(format!("{json}\n")).is_ok(),
            Err(_) => true,
        }
    };
    match ctx.runtime.status_report() {
        Ok(report) => {
            send(
                &tx,
                &SurfaceEvent::Hello {
                    report: Box::new(report),
                },
            );
        }
        Err(e) => {
            send(
                &tx,
                &SurfaceEvent::Error {
                    class: "internal".into(),
                    message: format!("status: {e}"),
                },
            );
        }
    }
    let fwd_tx = tx.clone();
    let forwarder = tokio::spawn(async move {
        loop {
            let ev = match sub.recv().await {
                Ok(ev) => ev,
                Err(RecvError::Lagged(n)) => SurfaceEvent::Notice {
                    text: format!("(missed {n} events)"),
                },
                Err(RecvError::Closed) => break,
            };
            if !send(&fwd_tx, &ev) {
                break;
            }
        }
    });
    loop {
        let line = match read_bounded_line(&mut reader, MAX_ATTACH_LINE).await {
            Ok(Some(l)) => l,
            Ok(None) | Err(_) => break,
        };
        let Ok(req) = serde_json::from_str::<serde_json::Value>(&line) else {
            continue;
        };
        // `status` answers this client with a fresh `hello`, ordered after
        // everything it said before (a piped chat waits for idle with it).
        if req.get("op").and_then(|o| o.as_str()) == Some("status") {
            if let Ok(report) = ctx.runtime.status_report() {
                send(
                    &tx,
                    &SurfaceEvent::Hello {
                        report: Box::new(report),
                    },
                );
            }
            continue;
        }
        let Some(text) = Some(req)
            .filter(|v| v.get("op").and_then(|o| o.as_str()) == Some("say"))
            .and_then(|v| v.get("text").and_then(|t| t.as_str()).map(str::to_string))
        else {
            continue;
        };
        let text = text.trim();
        if text.is_empty() {
            continue;
        }
        match ka_agent_channels::commands::route(&ctx.runtime, "console", text).await {
            ka_agent_channels::commands::Outcome::Reply(r) => {
                send(&tx, &SurfaceEvent::Reply { text: r });
            }
            ka_agent_channels::commands::Outcome::Silent => {}
            ka_agent_channels::commands::Outcome::NotACommand => {
                let expanded =
                    ka_agent_channels::refs::expand(&ctx.store, &ctx.runtime.workspace(), text)
                        .await;
                if let Err(e) = ctx.runtime.handle_message(&expanded, "console").await {
                    send(
                        &tx,
                        &SurfaceEvent::Error {
                            class: "internal".into(),
                            message: e.to_string(),
                        },
                    );
                }
            }
        }
    }
    forwarder.abort();
    drop(tx);
    let _ = writer.await;
    Ok(())
}

/// `ka-agent restart`: graceful stop over the admin socket. Under the
/// background service (`Restart=always` / `KeepAlive`) that is a restart.
pub fn restart_client() -> std::process::ExitCode {
    let paths = Paths::new(Paths::default_root());
    match admin::request(&paths, &serde_json::json!({"op": "shutdown"})) {
        Ok(_) => {
            println!(
                "Ka Agent is stopping — the background service starts it again \
                 (without one, start it with `ka-agent run`)"
            );
            std::process::ExitCode::SUCCESS
        }
        Err(admin::AdminError::NotRunning(_)) => {
            eprintln!("{}", not_running_text());
            std::process::ExitCode::FAILURE
        }
        Err(e) => {
            eprintln!("ka-agent: {e}");
            std::process::ExitCode::FAILURE
        }
    }
}

pub fn not_running_text() -> &'static str {
    "Ka Agent is not running — start it with `ka-agent service install` (background) or `ka-agent run`"
}

/// The operator's "now", in the zone a status report names.
pub fn zoned_now(tz_name: &str) -> jiff::Zoned {
    let tz = jiff::tz::TimeZone::get(tz_name).unwrap_or(jiff::tz::TimeZone::UTC);
    jiff::Timestamp::now().to_zoned(tz)
}

pub fn status_client() -> std::process::ExitCode {
    let paths = Paths::new(Paths::default_root());
    let v = match admin::request(&paths, &serde_json::json!({"op": "status"})) {
        Ok(v) => v,
        Err(admin::AdminError::NotRunning(_)) => {
            println!("{}", not_running_text());
            return std::process::ExitCode::FAILURE;
        }
        Err(e) => {
            eprintln!("ka-agent: {e}");
            return std::process::ExitCode::FAILURE;
        }
    };
    match serde_json::from_value::<ka_agent_runtime::StatusReport>(v["report"].clone()) {
        Ok(report) => {
            let profile = ka_agent_core::layout::profile_name();
            if profile != "default" && std::env::var_os("KA_AGENT_DATA_DIR").is_none() {
                println!("profile {profile}");
            }
            println!("{}", report.render(&zoned_now(&report.timezone)));
            std::process::ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("ka-agent: bad status reply: {e}");
            std::process::ExitCode::FAILURE
        }
    }
}
