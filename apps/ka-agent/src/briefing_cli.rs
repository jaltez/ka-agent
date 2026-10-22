//! `ka-agent briefing [on HH:MM | off | now]`: the daily briefing job,
//! edited in the store directly (the daemon's scheduler picks it up on its
//! next tick).

use std::process::ExitCode;

use ka_agent_runtime::briefing;
use ka_agent_runtime::paths::Paths;
use ka_agent_store::Store;

use crate::BriefingAction;

fn fail(msg: impl std::fmt::Display) -> ExitCode {
    eprintln!("ka-agent: {msg}");
    ExitCode::FAILURE
}

pub fn briefing_action(action: Option<BriefingAction>) -> ExitCode {
    let paths = Paths::new(Paths::default_root());
    if let Err(e) = paths.ensure_dirs() {
        return fail(format!("{}: {e}", paths.root().display()));
    }
    let store = match Store::open(&paths.state_db()) {
        Ok(s) => s,
        Err(e) => return fail(format!("{}: {e}", paths.state_db().display())),
    };
    let tz = ka_agent_core::DaemonConfig::load(&crate::default_config_path())
        .map(|c| c.time_zone())
        .unwrap_or_else(|_| jiff::tz::TimeZone::system());
    let now = jiff::Timestamp::now().to_zoned(tz);
    let running = crate::admin::is_running(&paths);
    match action {
        None => match briefing::find(&store) {
            Ok(Some(t)) => {
                let at = t.scheduled_at_ms.unwrap_or(0);
                println!(
                    "daily briefing on — next {}",
                    ka_agent_core::when::describe(at, &now)
                );
                ExitCode::SUCCESS
            }
            Ok(None) => {
                println!("daily briefing off — ka-agent briefing on 07:30");
                ExitCode::SUCCESS
            }
            Err(e) => fail(e),
        },
        Some(BriefingAction::On { time }) => match briefing::enable(&store, &time, &now) {
            Ok((_, first)) => {
                println!(
                    "daily briefing on — first {}",
                    ka_agent_core::when::describe(first, &now)
                );
                if !running {
                    println!("(it is sent while Ka is running — ka-agent service install)");
                }
                ExitCode::SUCCESS
            }
            Err(e) => fail(e),
        },
        Some(BriefingAction::Off) => match briefing::disable(&store) {
            Ok(true) => {
                println!("daily briefing off");
                ExitCode::SUCCESS
            }
            Ok(false) => {
                println!("no daily briefing was set");
                ExitCode::SUCCESS
            }
            Err(e) => fail(e),
        },
        Some(BriefingAction::Now) => match briefing::trigger_now(&store) {
            Ok(true) if running => {
                println!("briefing armed — it arrives within a few seconds");
                ExitCode::SUCCESS
            }
            Ok(true) => {
                println!("briefing armed — it is sent when Ka starts");
                ExitCode::SUCCESS
            }
            Ok(false) => fail("no daily briefing set up — ka-agent briefing on 07:30"),
            Err(e) => fail(e),
        },
    }
}
