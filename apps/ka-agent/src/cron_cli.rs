//! `ka-agent cron …`: scheduled prompts from the shell, edited in the store
//! (the daemon's scheduler picks changes up on its next tick), and
//! `ka-agent approvals …`, which needs the running daemon.

use std::process::ExitCode;

use ka_agent_runtime::cron_ops;
use ka_agent_runtime::paths::Paths;
use ka_agent_store::Store;

use crate::{ApprovalsAction, CronAction};

fn fail(msg: impl std::fmt::Display) -> ExitCode {
    eprintln!("ka-agent: {msg}");
    ExitCode::FAILURE
}

fn open() -> Result<(Paths, Store, jiff::Zoned), String> {
    let paths = Paths::new(Paths::default_root());
    paths
        .ensure_dirs()
        .map_err(|e| format!("{}: {e}", paths.root().display()))?;
    let store = Store::open(&paths.state_db())
        .map_err(|e| format!("{}: {e}", paths.state_db().display()))?;
    let tz = ka_agent_core::DaemonConfig::load(&crate::default_config_path())
        .map(|c| c.time_zone())
        .unwrap_or_else(|_| jiff::tz::TimeZone::system());
    Ok((paths, store, jiff::Timestamp::now().to_zoned(tz)))
}

pub fn cron_action(action: Option<CronAction>, json: bool) -> ExitCode {
    let (paths, store, now) = match open() {
        Ok(v) => v,
        Err(e) => return fail(e),
    };
    let tick = if crate::admin::is_running(&paths) {
        ""
    } else {
        " (fires while Ka is running)"
    };
    match action.unwrap_or(CronAction::List) {
        CronAction::List => {
            let crons = match store.cron_tasks() {
                Ok(c) => c,
                Err(e) => return fail(e),
            };
            if json {
                let rows: Vec<serde_json::Value> = crons
                    .iter()
                    .map(|t| {
                        let spec = t
                            .payload
                            .as_deref()
                            .and_then(ka_agent_core::CronSpec::from_payload);
                        serde_json::json!({
                            "id": t.id.as_str(),
                            "text": t.title,
                            "status": format!("{:?}", t.status).to_lowercase(),
                            "next_ms": t.scheduled_at_ms,
                            "spec": spec,
                        })
                    })
                    .collect();
                println!("{}", serde_json::json!({ "ok": true, "crons": rows }));
            } else if crons.is_empty() {
                println!("no crons — ka-agent cron add every weekday at 9 <prompt>");
            } else {
                println!("{}", ka_agent_channels::cron_list_text(&crons, &now));
            }
            ExitCode::SUCCESS
        }
        CronAction::Add { words } => {
            // `--if '<shell command>'` anywhere in the words (CLI form of
            // the /cron gate).
            let joined = words.join(" ");
            let (args, gate) = match split_gate(&joined) {
                Ok(v) => v,
                Err(e) => return fail(e),
            };
            let words: Vec<&str> = args.split_whitespace().collect();
            let (mut spec, first_at) =
                match ka_agent_core::CronSpec::from_words(&words, "operator", &now) {
                    Ok(v) => v,
                    Err(e) => return fail(e),
                };
            spec.gate = gate;
            let mut task = ka_agent_core::Task::new(spec.text.clone(), "cron", false);
            task.scheduled_at_ms = Some(first_at);
            task.payload = Some(spec.to_payload());
            if let Err(e) = store.insert_task(&task) {
                return fail(e);
            }
            println!(
                "cron {} scheduled — {}{tick}",
                ka_agent_core::ids::short_id(task.id.as_str()),
                ka_agent_core::when::describe(first_at, &now)
            );
            ExitCode::SUCCESS
        }
        CronAction::Rm { id } => op(&store, &id, |t| {
            cron_ops::remove(&store, t).map(|()| "removed".into())
        }),
        CronAction::Pause { id } => op(&store, &id, |t| {
            cron_ops::pause(&store, t).map(|()| "paused".into())
        }),
        CronAction::Resume { id } => op(&store, &id, |t| {
            cron_ops::resume(&store, t, &now).map(|next| {
                format!(
                    "resumed — next {}",
                    ka_agent_core::when::describe(next, &now)
                )
            })
        }),
        CronAction::Run { id } => op(&store, &id, |t| {
            cron_ops::trigger(&store, t).map(|()| format!("armed — fires on the next tick{tick}"))
        }),
        CronAction::Runs { id } => {
            let Ok(task) = cron_ops::resolve(&store, &id) else {
                return fail(format!("no live cron matches {id:?}"));
            };
            let runs = match store.runs_for(task.id.as_str(), 20) {
                Ok(r) => r,
                Err(e) => return fail(e),
            };
            if json {
                let rows: Vec<serde_json::Value> = runs
                    .iter()
                    .map(|r| serde_json::json!({"at_ms": r.started_ms, "status": r.status, "detail": r.error_message}))
                    .collect();
                println!("{}", serde_json::json!({ "ok": true, "runs": rows }));
            } else if runs.is_empty() {
                println!("no runs yet");
            } else {
                for r in runs {
                    let detail = r
                        .error_message
                        .map(|d| format!(" — {d}"))
                        .unwrap_or_default();
                    println!(
                        "{}  {}{detail}",
                        ka_agent_core::when::describe(r.started_ms, &now),
                        r.status
                    );
                }
            }
            ExitCode::SUCCESS
        }
    }
}

fn op(
    store: &Store,
    id: &str,
    f: impl FnOnce(&ka_agent_core::Task) -> Result<String, String>,
) -> ExitCode {
    let task = match cron_ops::resolve(store, id) {
        Ok(t) => t,
        Err(e) => return fail(e),
    };
    match f(&task) {
        Ok(what) => {
            println!(
                "cron {} {what}",
                ka_agent_core::ids::short_id(task.id.as_str())
            );
            ExitCode::SUCCESS
        }
        Err(e) => fail(e),
    }
}

/// `ka-agent approvals [list | y <id> | n <id>]`: the router's `/approvals`,
/// `/y`, `/n`, run inside the daemon (decisions answer live engine asks).
pub fn approvals_action(action: Option<ApprovalsAction>) -> ExitCode {
    let paths = Paths::new(Paths::default_root());
    let command = match action.unwrap_or(ApprovalsAction::List) {
        ApprovalsAction::List => "/approvals".to_string(),
        ApprovalsAction::Y { id } => format!("/y {id}"),
        ApprovalsAction::N { id } => format!("/n {id}"),
    };
    match crate::admin::request(
        &paths,
        &serde_json::json!({"op": "command", "text": command}),
    ) {
        Ok(v) => {
            println!("{}", v["reply"].as_str().unwrap_or(""));
            ExitCode::SUCCESS
        }
        Err(crate::admin::AdminError::NotRunning(_)) => {
            fail("Ka isn't running — approvals only exist while it is (ka-agent status)")
        }
        Err(e) => fail(e),
    }
}

/// Split a `--if '<command>'` flag out of the words.
fn split_gate(args: &str) -> Result<(String, Option<String>), String> {
    let Some(pos) = args.find(" --if ") else {
        return Ok((args.to_string(), None));
    };
    let (head, tail) = args.split_at(pos);
    let tail = tail.trim_start();
    let tail = tail.strip_prefix("--if").unwrap_or(tail).trim_start();
    let (command, rest) = if let Some(q) = tail.chars().next().filter(|c| *c == '\'' || *c == '"') {
        match tail[1..].find(q) {
            Some(end) => (tail[1..1 + end].to_string(), &tail[2 + end..]),
            None => return Err("unterminated --if quote".into()),
        }
    } else {
        match tail.split_once(' ') {
            Some((cmd, rest)) => (cmd.to_string(), rest),
            None => (tail.to_string(), ""),
        }
    };
    if command.trim().is_empty() {
        return Err("--if needs a shell command".into());
    }
    Ok((
        format!("{head} {}", rest.trim()).trim().to_string(),
        Some(command),
    ))
}
