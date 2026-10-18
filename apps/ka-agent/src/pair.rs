//! `ka-agent pair <telegram|matrix>`: issue a one-time pairing code and
//! wait for the operator's message from their phone.

use std::path::PathBuf;
use std::time::Duration;

use ka_agent_runtime::paths::Paths;
use ka_agent_store::Store;

use crate::admin;

#[derive(Debug, thiserror::Error)]
pub enum PairError {
    #[error("unknown channel {0:?} — use telegram or matrix")]
    UnknownChannel(String),
    #[error("{0} is not enabled — run `ka-agent setup {0}` first")]
    NotEnabled(String),
    #[error("start the daemon first (`ka-agent service install` or `ka-agent run`)")]
    NoDaemon,
}

pub fn pair(channel: String, config: PathBuf) -> std::process::ExitCode {
    match run(channel, config) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("ka-agent: {e}");
            std::process::ExitCode::FAILURE
        }
    }
}

fn run(channel: String, config: PathBuf) -> Result<std::process::ExitCode, PairError> {
    if !matches!(channel.as_str(), "telegram" | "matrix") {
        return Err(PairError::UnknownChannel(channel));
    }
    let cfg = ka_agent_core::DaemonConfig::load(&config)
        .map_err(|_| PairError::NotEnabled(channel.clone()))?;
    let enabled = match channel.as_str() {
        "telegram" => cfg.telegram.enabled,
        _ => cfg.matrix.enabled,
    };
    if !enabled {
        return Err(PairError::NotEnabled(channel.clone()));
    }
    let paths = Paths::new(Paths::default_root());
    let store = Store::open(&paths.state_db()).map_err(|_| PairError::NoDaemon)?;
    let code = ka_agent_channels::pairing::issue(&store, &channel)
        .map_err(|e| PairError::NotEnabled(format!("{channel} ({e})")))?;
    if channel == "telegram" && !admin::is_running(&paths) {
        // Standalone wait: talk to Bot API directly, no daemon needed.
        let base = cfg.telegram.api_base().to_string();
        let token = ka_agent_channels::secret::resolve(
            cfg.telegram
                .token_secret
                .as_deref()
                .unwrap_or("telegram.bot_token"),
            &paths.root().join("secrets"),
        )
        .map_err(|_| PairError::NotEnabled("telegram (no bot token resolved)".into()))?;
        println!("Send this to your bot within 10 minutes:  /pair {code}");
        match standalone_wait(&base, &token, &store) {
            Ok((uid, name)) => {
                println!("✓ paired {uid}:{name}");
                return Ok(std::process::ExitCode::SUCCESS);
            }
            Err(e) => {
                println!("code expired — run `ka-agent pair telegram` again ({e})");
                return Ok(std::process::ExitCode::FAILURE);
            }
        }
    }
    if !admin::is_running(&paths) {
        return Err(PairError::NoDaemon);
    }
    println!("Send this to your bot within 10 minutes:  /pair {code}");
    // The running daemon redeems the code; watch for the new identity row.
    let deadline = std::time::Instant::now() + Duration::from_secs(600);
    while std::time::Instant::now() < deadline {
        if let Ok(rows) = store.list_identities()
            && rows.iter().any(|r| r.channel == channel)
        {
            let address = rows
                .iter()
                .find(|r| r.channel == channel)
                .map(|r| r.address.clone())
                .unwrap_or_default();
            println!("✓ paired {channel}:{address}");
            return Ok(std::process::ExitCode::SUCCESS);
        }
        std::thread::sleep(Duration::from_secs(1));
    }
    println!("code expired — run `ka-agent pair {channel}` again");
    Ok(std::process::ExitCode::FAILURE)
}

/// The standalone telegram wait on the caller's runtime (main runs `pair`
/// on a current-thread runtime when the daemon is not running).
fn standalone_wait(base: &str, token: &str, store: &Store) -> Result<(i64, String), String> {
    // Called from `run`, which may be sync; run on a tiny throwaway runtime.
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| e.to_string())?
        .block_on(ka_agent_channels::pairing::wait_telegram_standalone(
            base,
            token,
            store,
            Duration::from_secs(600),
        ))
}
