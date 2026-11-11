//! `ka-agent dashboard`: one-shot login URL for the web control panel,
//! opened in the browser. Replaces the old `web-token` CLI dance.

use std::path::PathBuf;

use ka_agent_core::DaemonConfig;

use ka_agent_runtime::paths::Paths;

use crate::admin;

fn fail(msg: &str) -> std::process::ExitCode {
    eprintln!("ka-agent: {msg}");
    std::process::ExitCode::FAILURE
}

/// The daemon is the source of truth: it publishes `web.url` when the UI
/// bound, so no config read is needed here.
pub fn dashboard(config: PathBuf, no_open: bool) -> std::process::ExitCode {
    let paths = Paths::new(Paths::default_root());
    if !admin::is_running(&paths) {
        return fail("start Ka Agent first: ka-agent service install (or ka-agent run)");
    }
    let Ok(store) = ka_agent_store::Store::open(&paths.state_db()) else {
        return fail("cannot open the state store");
    };
    let url = match store.meta_get("web.url").ok().flatten() {
        Some(u) if !u.is_empty() => u,
        _ => {
            // The daemon only publishes `web.url` when the listener bound:
            // with web enabled in the config, a missing URL means the
            // bind failed (port busy), not that the UI is off.
            let enabled = DaemonConfig::load(&config)
                .map(|c| c.web.enabled)
                .unwrap_or(true);
            return if enabled {
                fail(
                    "the web UI failed to start (port busy?) — try `ka-agent config set web.port <n>`, \
                     then `ka-agent restart`; details: `ka-agent logs`",
                )
            } else {
                fail("the web UI is off — ka-agent config set web.enabled true")
            };
        }
    };
    match ka_agent_web::issue_auth_url_at(&store, &url_base(&url)) {
        Ok(link) => {
            println!("open: {link}");
            if no_open {
                return std::process::ExitCode::SUCCESS;
            }
            let opened = open_browser(&link);
            if !opened {
                println!(
                    "(couldn't launch a browser — paste the URL above; on WSL set BROWSER or use wslview)"
                );
            }
            std::process::ExitCode::SUCCESS
        }
        Err(e) => fail(&e.to_string()),
    }
}

/// `scheme://authority` of a published URL (path/query dropped).
fn url_base(url: &str) -> String {
    match url.split_once("://") {
        Some((scheme, rest)) => {
            let authority = rest.split('/').next().unwrap_or(rest);
            format!("{scheme}://{authority}")
        }
        None => url.to_string(),
    }
}

/// Launch `url` in the operator's browser. Returns whether a launcher was
/// started; every child writes to /dev/null, so a broken helper (WSL's
/// `wslview` shims, a headless `xdg-open`) cannot spam the terminal.
fn open_browser(url: &str) -> bool {
    // The standard escape hatch wins: BROWSER may carry arguments
    // (`firefox --new-tab`, an absolute path, a wrapper).
    if let Ok(browser) = std::env::var("BROWSER") {
        let mut parts = browser.split_whitespace();
        if let Some(bin) = parts.next() {
            let mut args: Vec<&str> = parts.collect();
            args.push(url);
            if try_spawn(bin, &args) {
                return true;
            }
        }
    }
    for (bin, args) in launcher_candidates(url) {
        let refs: Vec<&str> = args.iter().map(String::as_str).collect();
        if try_spawn(&bin, &refs) {
            return true;
        }
    }
    false
}

/// Launch candidates in order for this platform. WSL puts the Windows
/// browser first: the `wslview`/`xdg-open` shims are unreliable (they can
/// fail as root and print their own diagnostics).
fn launcher_candidates(url: &str) -> Vec<(String, Vec<String>)> {
    let mut out: Vec<(String, Vec<String>)> = Vec::new();
    #[cfg(target_os = "macos")]
    {
        out.push(("open".into(), vec![url.into()]));
    }
    #[cfg(windows)]
    {
        out.push((
            "cmd".into(),
            vec!["/C".into(), "start".into(), "".into(), url.into()],
        ));
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        if is_wsl() {
            out.push((
                "/mnt/c/Windows/System32/cmd.exe".into(),
                vec!["/C".into(), "start".into(), "".into(), url.into()],
            ));
            out.push((
                "cmd.exe".into(),
                vec!["/C".into(), "start".into(), "".into(), url.into()],
            ));
            out.push((
                "powershell.exe".into(),
                vec![
                    "-NoProfile".into(),
                    "-Command".into(),
                    format!("Start-Process '{url}'"),
                ],
            ));
            out.push(("wslview".into(), vec![url.into()]));
        }
        out.push(("xdg-open".into(), vec![url.into()]));
        out.push(("gio".into(), vec!["open".into(), url.into()]));
    }
    out
}

/// WSL exposes the kernel string `microsoft` (or sets WSL_DISTRO_NAME).
fn is_wsl() -> bool {
    std::env::var_os("WSL_DISTRO_NAME").is_some()
        || std::fs::read_to_string("/proc/version")
            .map(|v| v.to_ascii_lowercase().contains("microsoft"))
            .unwrap_or(false)
}

/// Spawn one launcher with every stream detached. An absolute path must
/// exist (a missing `/mnt/c/...` is common off-Windows); a bare name is
/// resolved through PATH by the OS.
fn try_spawn(bin: &str, args: &[&str]) -> bool {
    let p = std::path::Path::new(bin);
    if p.is_absolute() && !p.exists() {
        return false;
    }
    std::process::Command::new(bin)
        .args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .is_ok()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[test]
    fn a_missing_absolute_launcher_is_never_spawned() {
        assert!(!try_spawn("/nonexistent/ka-browser-xyz", &["http://x"]));
    }

    #[cfg(unix)]
    #[test]
    fn a_real_binary_spawns() {
        assert!(try_spawn("/bin/true", &[]));
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    #[test]
    fn wsl_launches_the_windows_browser_before_the_shims() {
        let candidates = launcher_candidates("http://127.0.0.1:8480/");
        if is_wsl() {
            let (bin, args) = &candidates[0];
            assert!(
                bin.ends_with("cmd.exe") && args.iter().any(|a| a == "start"),
                "first WSL launcher: {bin} {args:?}"
            );
            let xdg = candidates.iter().position(|(b, _)| b == "xdg-open");
            assert!(xdg.is_none_or(|i| i > 0), "cmd.exe precedes xdg-open");
        } else {
            assert_eq!(candidates[0].0, "xdg-open");
        }
    }

    #[test]
    fn url_base_drops_path_and_query() {
        assert_eq!(
            url_base("https://agent.example.com/auth?token=x"),
            "https://agent.example.com"
        );
        assert_eq!(url_base("http://127.0.0.1:8480"), "http://127.0.0.1:8480");
    }
}
