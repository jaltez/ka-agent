//! `ka-agent` — the Ka Agent binary: local chat surface (M1), the resident
//! daemon with its admin socket (M2), doctor, `status` client, and the hidden
//! `__worker` subcommand the runtime supervises.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use clap::{Parser, Subcommand};
use ka_agent_core::{ApprovalDecision, ApprovalStatus, DaemonConfig, SessionStatus};
use ka_agent_port::{WorkerPort, WorkerRun, run_worker};
use ka_agent_runtime::surface::ApprovalCard;
use ka_agent_runtime::{Runtime, Surface, paths::Paths};
use ka_agent_store::Store;

#[derive(Parser)]
#[command(
    name = "ka-agent",
    version,
    about = "Ka Agent — resident assistant on the Ka engine"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Local chat surface over the runtime (Telegram arrives in M2).
    Chat {
        /// Config path (default: ~/.config/ka-agent/ka-agent.toml).
        #[arg(short, long)]
        config: Option<PathBuf>,
        /// Hidden: run the engine with a scripted speaker (e2e tests).
        #[arg(long, hide = true)]
        scenario: Option<PathBuf>,
    },
    /// Run the resident daemon: runtime + channels + admin socket.
    #[command(alias = "daemon")]
    Run {
        /// Config path (default: ~/.config/ka-agent/ka-agent.toml).
        /// Invalid config exits 1 (no fallback to defaults).
        #[arg(short, long)]
        config: Option<PathBuf>,
        /// Hidden test hook: run the engine worker with a scripted speaker.
        #[arg(long, hide = true)]
        scenario: Option<PathBuf>,
    },
    /// Query a running daemon over its admin socket.
    Status {
        /// Config path (default: ~/.config/ka-agent/ka-agent.toml).
        #[arg(short, long)]
        config: Option<PathBuf>,
    },
    /// Instance health report.
    Doctor {
        #[arg(short, long)]
        config: Option<PathBuf>,
    },
    /// Hidden: the supervised worker process (spawned by the runtime).
    #[command(name = "__worker", hide = true)]
    Worker {
        /// Default scripted-speaker scenario (e2e; Open can override).
        #[arg(long, hide = true)]
        scenario: Option<PathBuf>,
    },
}

fn config_path(flag: &Option<PathBuf>) -> PathBuf {
    flag.clone().unwrap_or_else(|| {
        if let Some(p) = std::env::var_os("KA_AGENT_CONFIG") {
            return PathBuf::from(p);
        }
        let base = std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                let home = std::env::var_os("HOME").unwrap_or_default();
                PathBuf::from(home).join(".config")
            });
        base.join("ka-agent/ka-agent.toml")
    })
}

fn load_config(path: &Path) -> DaemonConfig {
    match DaemonConfig::load(path) {
        Ok(cfg) => cfg,
        Err(e) => {
            eprintln!(
                "ka-agent: config ({}) invalid, using defaults: {e}",
                path.display()
            );
            DaemonConfig::default()
        }
    }
}

/// The console surface: everything the runtime emits, printed.
struct ConsoleSurface;

impl Surface for ConsoleSurface {
    fn on_turn_started(&self) {
        println!();
    }

    fn on_delta_text(&self, text: &str) {
        print!("{text}");
    }

    fn on_note(&self, text: &str) {
        println!("\n· {text}");
    }

    fn on_approval(&self, card: &ApprovalCard) {
        println!(
            "\n┌ approval {} (expires {})\n│ {}",
            card.approval_id,
            crate::fmt_ts(card.expires_at_ms),
            card.question
        );
        println!("└ approve: /y   deny: /n");
    }

    fn on_approval_settled(&self, approval_id: &str, status: ApprovalStatus) {
        println!("\n· approval {approval_id} → {status:?}");
    }

    fn on_turn_finished(&self, stop: &str, cost_usd: f64, tokens: u64) {
        println!("\n· turn {stop} · ${cost_usd:.4} · {tokens} tokens");
    }

    fn on_session_status(&self, status: SessionStatus) {
        println!("· session {status:?}");
    }

    fn on_error(&self, class: &str, message: &str) {
        println!("\n! [{class}] {message}");
    }
}

fn main() -> std::process::ExitCode {
    let cli = Cli::parse();
    match cli.command {
        // The worker builds its own current-thread runtime inside
        // `run_worker`; it must not run inside another tokio runtime.
        Command::Worker { scenario } => match run_worker(WorkerRun { scenario }) {
            Ok(()) => std::process::ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("ka-agent worker: {e}");
                std::process::ExitCode::FAILURE
            }
        },
        Command::Chat { config, scenario } => {
            let Ok(rt) = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
            else {
                eprintln!("ka-agent: cannot build async runtime");
                return std::process::ExitCode::FAILURE;
            };
            rt.block_on(chat(config_path(&config), scenario))
        }
        Command::Run { config, scenario } => {
            let Ok(rt) = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
            else {
                eprintln!("ka-agent: cannot build async runtime");
                return std::process::ExitCode::FAILURE;
            };
            rt.block_on(run_daemon(config_path(&config), scenario))
        }
        Command::Status { config } => status_client(config_path(&config)),
        Command::Doctor { config } => doctor(config_path(&config)),
    }
}

fn fmt_ts(ms: i64) -> String {
    // Local, dependency-free: minutes resolution is enough for a card.
    let mins = ms.div_euclid(60_000);
    let (h, m) = (mins.div_euclid(60) % 24, mins % 60);
    format!("{h:02}:{m:02}")
}

async fn chat(config: PathBuf, scenario: Option<PathBuf>) -> std::process::ExitCode {
    let cfg = load_config(&config);
    let paths = Paths::new(Paths::default_root());
    let store = match Store::open(&paths.state_db()) {
        Ok(s) => Arc::new(s),
        Err(e) => {
            eprintln!("ka-agent: cannot open {}: {e}", paths.state_db().display());
            return std::process::ExitCode::FAILURE;
        }
    };
    let surface = Arc::new(ConsoleSurface);
    let mut port = WorkerPort::with_current_exe();
    if let Some(sc) = scenario {
        port.scenario_override = Some(sc);
    }
    let runtime = match Runtime::new(
        cfg,
        paths.clone(),
        store,
        port,
        Arc::clone(&surface) as Arc<dyn Surface>,
    ) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("ka-agent: runtime init failed: {e}");
            return std::process::ExitCode::FAILURE;
        }
    };
    println!(
        "ka-agent chat · session {} · data {}",
        runtime.session_id(),
        paths.root().display()
    );
    println!("commands: /help /status /new /stop /y /n /quit");

    let expiry = runtime.clone();
    tokio::spawn(async move {
        expiry
            .run_expiry_timer(std::time::Duration::from_secs(30))
            .await;
    });

    let stdin = std::io::stdin();
    let mut line = String::new();
    loop {
        line.clear();
        print!("> ");
        use std::io::Write;
        let _ = std::io::stdout().flush();
        if stdin.read_line(&mut line).unwrap_or(0) == 0 {
            break;
        }
        let text = line.trim();
        if text.is_empty() {
            continue;
        }
        match text {
            "/quit" | "/q" | "/exit" => break,
            "/help" => {
                println!(
                    "/status — session, strand, budget\n\
                     /new — fresh session (drops the live worker)\n\
                     /stop — abort the running turn (grace, then kill)\n\
                     /y /n — approve / deny the pending approval\n\
                     /quit — exit"
                );
            }
            "/status" => {
                let (status, strand, usd, tokens, crashes) = runtime.status();
                println!(
                    "session {} · {status:?}\nstrand: {}\nbudget: ${usd:.4} + {tokens} tokens · recent worker crashes: {crashes}",
                    runtime.session_id(),
                    strand.as_deref().unwrap_or("(new)")
                );
            }
            "/new" => match runtime.new_session().await {
                Ok(id) => println!("· new session {id}"),
                Err(e) => println!("! {e}"),
            },
            "/stop" => {
                if let Err(e) = runtime.stop_turn().await {
                    println!("! {e}");
                }
            }
            "/y" | "/n" => {
                let decision = if text == "/y" {
                    ApprovalDecision::Allow
                } else {
                    ApprovalDecision::Deny
                };
                match runtime.decide_latest(decision).await {
                    Ok(true) => {}
                    Ok(false) => println!("· no pending approval"),
                    Err(e) => println!("! {e}"),
                }
            }
            _ => {
                if let Err(e) = runtime.handle_message(text, "console").await {
                    println!("! {e}");
                }
            }
        }
    }
    std::process::ExitCode::SUCCESS
}

fn doctor(config: PathBuf) -> std::process::ExitCode {
    let cfg = DaemonConfig::load(&config).unwrap_or_else(|e| {
        eprintln!("config: {e} (continuing with defaults)");
        DaemonConfig::default()
    });
    let paths = Paths::new(Paths::default_root());
    println!("ka-agent {}", env!("CARGO_PKG_VERSION"));
    println!("ka pin: rev a56aae2d1b3a4c31c4910979e1033002eacc53a0");
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
    std::process::ExitCode::SUCCESS
}

/// The no-channel surface: local observability only. Telegram, when enabled,
/// is the real surface; this keeps a daemon without channels from being mute.
struct NullSurface;

impl Surface for NullSurface {
    fn on_note(&self, text: &str) {
        println!("· {text}");
    }

    fn on_approval(&self, card: &ApprovalCard) {
        println!(
            "┌ approval {} (expires {})\n│ {}",
            card.approval_id,
            fmt_ts(card.expires_at_ms),
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

async fn run_daemon(config: PathBuf, scenario: Option<PathBuf>) -> std::process::ExitCode {
    // Strict, unlike chat: a daemon with a broken config must not silently
    // run with defaults.
    let cfg = match DaemonConfig::load(&config) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("ka-agent: config ({}) invalid: {e}", config.display());
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
                     set KA_AGENT_SECRET_TELEGRAM_BOT_TOKEN or place a 0600 file at \
                     {}/secrets/{secret_name}",
                    paths.root().display()
                );
                return std::process::ExitCode::FAILURE;
            }
        }
    } else {
        None
    };
    // Surface: Telegram when enabled, otherwise local-print NullSurface.
    let surface: Arc<dyn Surface> = match &token {
        Some(t) => Arc::new(ka_agent_channels::TelegramSurface::new(
            cfg.telegram.api_base().to_string(),
            t.clone(),
        )),
        None => Arc::new(NullSurface),
    };

    let mut port = WorkerPort::with_current_exe();
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
    println!(
        "ka-agent run · session {} · data {}",
        runtime.session_id(),
        paths.root().display()
    );

    let expiry = runtime.clone();
    tokio::spawn(async move {
        expiry
            .run_expiry_timer(std::time::Duration::from_secs(30))
            .await;
    });

    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);

    // Telegram channel (inbound) when enabled.
    if let Some(token) = token {
        let tcfg = ka_agent_channels::TelegramConfig::from_config(&cfg, token);
        let channel =
            ka_agent_channels::TelegramChannel::new(tcfg, runtime.clone(), Arc::clone(&store));
        tokio::spawn(channel.run(shutdown_rx.clone()));
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

    let admin_runtime = runtime.clone();
    let admin_store = Arc::clone(&store);
    let admin = tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                continue;
            };
            eprintln!("ka-agent: admin connection");
            let rt = admin_runtime.clone();
            let st = Arc::clone(&admin_store);
            tokio::spawn(async move {
                if let Err(e) = serve_admin(stream, rt, st).await {
                    eprintln!("ka-agent: admin request: {e}");
                }
            });
        }
    });

    // Graceful shutdown: SIGINT or SIGTERM, whichever first.
    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .map_err(|e| eprintln!("ka-agent: cannot watch SIGTERM: {e}"))
        .ok();
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        _ = async {
            match term.as_mut() {
                Some(s) => {
                    let _ = s.recv().await;
                }
                None => std::future::pending::<()>().await,
            }
        } => {}
    }
    eprintln!("ka-agent: shutting down");
    let _ = shutdown_tx.send(true);
    admin.abort();
    // Give the Telegram channel a moment to finish its in-flight poll.
    tokio::time::sleep(std::time::Duration::from_secs(3)).await;
    std::process::ExitCode::SUCCESS
}

async fn serve_admin(
    mut stream: tokio::net::UnixStream,
    runtime: Runtime,
    store: Arc<Store>,
) -> std::io::Result<()> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let peer_uid = stream.peer_cred().map(|c| c.uid()).unwrap_or(u32::MAX);
    let mut line = Vec::new();
    let mut buf = [0u8; 1024];
    loop {
        let n = stream.read(&mut buf).await?;
        if n == 0 {
            return Ok(());
        }
        line.extend_from_slice(&buf[..n]);
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
            Some(_) => serde_json::json!({"error": "unknown op"}),
            None => serde_json::json!({"error": "bad json"}),
        }
    };
    let (r, mut w) = stream.into_split();
    drop(r);
    w.write_all(format!("{reply}\n").as_bytes()).await?;
    w.flush().await
}

fn status_client(config: PathBuf) -> std::process::ExitCode {
    // Defaults are fine here: the socket path depends only on the data root.
    let _cfg = load_config(&config);
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
