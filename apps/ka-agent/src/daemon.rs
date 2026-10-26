//! The resident daemon: `ka-agent run` (a [`Host`] with channels + web +
//! admin socket) and the local admin clients (`status`, `restart`).

use std::path::{Path, PathBuf};

use ka_agent_runtime::Envelope;
use ka_agent_runtime::event::AgentEvent;
use ka_agent_runtime::surface::Surface;

use crate::admin;
use crate::host::{Host, HostOptions};

/// The journal surface: every runtime event as one stderr line, so the
/// service log (`ka-agent logs`) is a complete trail of what happened.
pub struct LogSurface;

impl Surface for LogSurface {
    fn emit(&self, env: &Envelope) {
        match &env.ev {
            AgentEvent::TurnStarted { turn, source } => {
                eprintln!("· turn started ({source}) {turn}");
            }
            AgentEvent::ToolFinished {
                tool,
                ok,
                ms,
                error,
                ..
            } => match (ok, error) {
                (true, _) => eprintln!("· tool {tool} ok · {ms}ms"),
                (false, Some(err)) => eprintln!("· tool {tool} failed · {ms}ms · {err}"),
                (false, None) => eprintln!("· tool {tool} failed · {ms}ms"),
            },
            AgentEvent::TurnFinished {
                stop,
                cost_usd,
                tokens,
                ..
            } => eprintln!("· turn {stop} · ${cost_usd:.4} · {tokens} tokens"),
            AgentEvent::Notice { text } => eprintln!("» {text}"),
            AgentEvent::Error { class, message } => eprintln!("! [{class}] {message}"),
            AgentEvent::Approval { id, question, .. } => eprintln!("» approval {id}: {question}"),
            _ => {}
        }
    }
}

/// The exact refusal when another runtime holds the data root.
pub fn lock_held_text(root: &Path) -> String {
    format!(
        "ka-agent: another Ka Agent is already running on {} (the daemon or a local chat).\n\
         check it with `ka-agent status`; stop the daemon with `ka-agent restart` or `ka-agent service stop`",
        root.display()
    )
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

    let host = match Host::start(
        &config,
        HostOptions {
            channels: true,
            web_port,
            scenario,
        },
    ) {
        Ok(h) => h,
        Err(e) => {
            eprintln!("{e}");
            return std::process::ExitCode::FAILURE;
        }
    };

    // Graceful shutdown: SIGINT, SIGTERM, or the admin socket's `shutdown`
    // op (which is what `ka-agent restart` drives). The streams were
    // registered before any channel started.
    let mut shutdown_rx = host.shutdown_rx();
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
    host.shutdown();
    // Give the Telegram channel a moment to finish its in-flight poll.
    tokio::time::sleep(std::time::Duration::from_secs(3)).await;
    std::process::ExitCode::SUCCESS
}

/// `ka-agent restart`: graceful stop over the admin socket. Under the
/// background service (`Restart=always` / `KeepAlive`) that is a restart.
pub fn restart_client() -> std::process::ExitCode {
    let paths = ka_agent_runtime::paths::Paths::new(ka_agent_runtime::paths::Paths::default_root());
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

pub fn status_client(json: bool) -> std::process::ExitCode {
    let paths = ka_agent_runtime::paths::Paths::new(ka_agent_runtime::paths::Paths::default_root());
    let fail_json = |error: &str| {
        println!("{}", serde_json::json!({"ok": false, "error": error}));
        std::process::ExitCode::FAILURE
    };
    let v = match admin::request(&paths, &serde_json::json!({"op": "status"})) {
        Ok(v) => v,
        Err(admin::AdminError::NotRunning(_)) if json => return fail_json("not running"),
        Err(admin::AdminError::NotRunning(_)) => {
            println!("{}", not_running_text());
            return std::process::ExitCode::FAILURE;
        }
        Err(e) if json => return fail_json(&e.to_string()),
        Err(e) => {
            eprintln!("ka-agent: {e}");
            return std::process::ExitCode::FAILURE;
        }
    };
    if json {
        println!(
            "{}",
            serde_json::json!({
                "ok": true,
                "profile": ka_agent_core::layout::profile_name(),
                "report": v["report"],
            })
        );
        return std::process::ExitCode::SUCCESS;
    }
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
