//! `ka-agent dashboard`: one-shot login URL for the web control panel,
//! opened in the browser. Replaces the old `web-token` CLI dance.

use std::path::PathBuf;

use ka_agent_runtime::paths::Paths;

use crate::admin;

fn fail(msg: &str) -> std::process::ExitCode {
    eprintln!("ka-agent: {msg}");
    std::process::ExitCode::FAILURE
}

/// The daemon is the source of truth: it publishes `web.url` when the UI
/// bound, so no config read is needed here.
pub fn dashboard(_config: PathBuf, no_open: bool) -> std::process::ExitCode {
    let paths = Paths::new(Paths::default_root());
    if !admin::is_running(&paths) {
        return fail("start Ka Agent first: ka-agent service install (or ka-agent run)");
    }
    let Ok(store) = ka_agent_store::Store::open(&paths.state_db()) else {
        return fail("cannot open the state store");
    };
    let url = match store.meta_get("web.url").ok().flatten() {
        Some(u) if !u.is_empty() => u,
        _ => return fail("the web UI is off — ka-agent config set web.enabled true"),
    };
    match ka_agent_web::issue_auth_url(&store, url_port(&url)) {
        Ok(link) => {
            println!("open: {link}");
            if no_open {
                return std::process::ExitCode::SUCCESS;
            }
            open_browser(&link);
            println!("(if nothing opened, paste the URL above into your browser)");
            std::process::ExitCode::SUCCESS
        }
        Err(e) => fail(&e.to_string()),
    }
}

fn url_port(url: &str) -> u16 {
    url.rsplit(':')
        .next()
        .and_then(|p| p.parse().ok())
        .unwrap_or(8480)
}

fn open_browser(url: &str) {
    #[cfg(target_os = "macos")]
    let _ = std::process::Command::new("open").arg(url).spawn();
    #[cfg(all(unix, not(target_os = "macos")))]
    let _ = std::process::Command::new("xdg-open").arg(url).spawn();
    #[cfg(windows)]
    let _ = std::process::Command::new("cmd")
        .args(["/C", "start", "", url])
        .spawn();
}
