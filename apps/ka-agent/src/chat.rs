//! The local console surface: `ka-agent chat`. One process, one session,
//! streaming output — and the same shared command router every channel
//! uses, so `/status` means exactly what it means on Telegram.

use std::path::PathBuf;
use std::sync::Arc;

use ka_agent_core::{ApprovalStatus, SessionStatus};
use ka_agent_runtime::surface::ApprovalCard;
use ka_agent_runtime::{Runtime, Surface, paths::Paths};
use ka_agent_store::Store;

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
            "\n┌ approval {} (expires in {})\n│ {}",
            card.approval_id,
            ka_agent_channels::commands::fmt_interval(card.expires_at_ms - ka_agent_core::now_ms()),
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

pub async fn chat(config: PathBuf, scenario: Option<PathBuf>) -> std::process::ExitCode {
    // Strict, like the daemon: a console running on silently-defaulted
    // config would write to the real data root with a workspace the
    // operator never chose.
    let cfg = match ka_agent_core::DaemonConfig::load(&config) {
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
    let store = match Store::open(&paths.state_db()) {
        Ok(s) => Arc::new(s),
        Err(e) => {
            eprintln!("ka-agent: cannot open {}: {e}", paths.state_db().display());
            return std::process::ExitCode::FAILURE;
        }
    };
    let surface = Arc::new(ConsoleSurface);
    let mut port = ka_agent_port::WorkerPort::with_current_exe();
    if let Some(sc) = scenario {
        port.scenario_override = Some(sc);
    }
    let runtime = match Runtime::new(
        cfg,
        paths.clone(),
        Arc::clone(&store),
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
    // No-agent cron handlers (M11): the calendar digest runs without the
    // engine. Registered right after the runtime exists, before any tick.
    {
        let st = Arc::clone(&store);
        runtime.set_no_agent_handler(
            "calendar",
            Arc::new(move |src| ka_agent_channels::calendar::digest(src, &st)),
        );
    }
    println!("{}", ka_agent_channels::commands::HELP);
    println!("· console only: /quit exits");

    let maintenance = runtime.clone();
    tokio::spawn(async move {
        maintenance
            .run_maintenance(std::time::Duration::from_secs(15))
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
        if matches!(text, "/quit" | "/q" | "/exit") {
            break;
        }
        // The shared router owns every `/command`; only free text reaches
        // the engine (with `@references` expanded first, like a channel).
        match ka_agent_channels::commands::route(&runtime, "console", text).await {
            ka_agent_channels::commands::Outcome::Reply(reply) => println!("{reply}"),
            ka_agent_channels::commands::Outcome::Silent => {}
            ka_agent_channels::commands::Outcome::NotACommand => {
                let expanded =
                    ka_agent_channels::refs::expand(&store, &runtime.workspace(), text).await;
                if let Err(e) = runtime.handle_message(&expanded, "console").await {
                    println!("! {e}");
                }
            }
        }
    }
    std::process::ExitCode::SUCCESS
}
