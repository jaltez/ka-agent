//! `ka-agent` — the Ka Agent binary: local chat surface (M1), doctor, and
//! the hidden `__worker` subcommand the runtime supervises.

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
