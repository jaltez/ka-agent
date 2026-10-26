//! `ka-agent notify|ask|hooks` (plan 2.7): the local triggers a script or
//! editor uses to reach the running agent, and hook introspection.

use std::path::PathBuf;

use ka_agent_core::DaemonConfig;
use ka_agent_runtime::paths::Paths;

use crate::admin;
use crate::cli::HooksAction;

fn read_text(words: &[String]) -> Result<String, String> {
    let joined = words.join(" ");
    let text = if joined.trim() == "-" {
        let mut buf = String::new();
        std::io::Read::read_to_string(&mut std::io::stdin(), &mut buf)
            .map_err(|e| format!("stdin: {e}"))?;
        buf
    } else {
        joined
    };
    let text = text.trim().to_string();
    if text.is_empty() {
        return Err("empty text".into());
    }
    if text.len() > 8 * 1024 {
        return Err("text is over 8 KiB".into());
    }
    Ok(text)
}

fn running(paths: &Paths) -> bool {
    admin::is_running(paths)
}

pub fn notify(text: &[String], _config: PathBuf) -> std::process::ExitCode {
    let paths = Paths::new(Paths::default_root());
    let text = match read_text(text) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("ka-agent: {e}");
            return std::process::ExitCode::FAILURE;
        }
    };
    if !running(&paths) {
        eprintln!("ka-agent: not running — start it with `ka-agent run`");
        return std::process::ExitCode::FAILURE;
    }
    match admin::request(&paths, &serde_json::json!({"op": "notify", "text": text})) {
        Ok(v) if v.get("ok").and_then(serde_json::Value::as_bool) == Some(true) => {
            std::process::ExitCode::SUCCESS
        }
        Ok(v) => {
            eprintln!("ka-agent: {v}");
            std::process::ExitCode::FAILURE
        }
        Err(e) => {
            eprintln!("ka-agent: {e}");
            std::process::ExitCode::FAILURE
        }
    }
}

pub fn ask(
    text: &[String],
    wait: bool,
    background: bool,
    _config: PathBuf,
) -> std::process::ExitCode {
    let paths = Paths::new(Paths::default_root());
    let text = match read_text(text) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("ka-agent: {e}");
            return std::process::ExitCode::FAILURE;
        }
    };
    if !running(&paths) {
        eprintln!("ka-agent: not running — start it with `ka-agent run`");
        return std::process::ExitCode::FAILURE;
    }
    let request = serde_json::json!({
        "op": "ask",
        "text": text,
        "background": background,
        "wait": wait,
    });
    match admin::request(&paths, &request) {
        Ok(v) if v.get("ok").and_then(serde_json::Value::as_bool) == Some(true) => {
            std::process::ExitCode::SUCCESS
        }
        Ok(v) => {
            eprintln!("ka-agent: {v}");
            std::process::ExitCode::FAILURE
        }
        Err(e) => {
            eprintln!("ka-agent: {e}");
            std::process::ExitCode::FAILURE
        }
    }
}

pub fn hooks(action: HooksAction, config: PathBuf) -> std::process::ExitCode {
    let cfg = match DaemonConfig::load(&config) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("ka-agent: {}", crate::config_problem(&config, &e));
            return std::process::ExitCode::FAILURE;
        }
    };
    match action {
        HooksAction::List => {
            if cfg.hooks.is_empty() {
                println!("no hooks (add [[hooks]] to {})", config.display());
                return std::process::ExitCode::SUCCESS;
            }
            for h in &cfg.hooks {
                let class = if ka_agent_core::config::ENGINE_HOOK_EVENTS.contains(&h.on.as_str()) {
                    "engine"
                } else {
                    "agent"
                };
                let tool = h
                    .tool
                    .as_deref()
                    .map(|t| format!(" tool={t}"))
                    .unwrap_or_default();
                println!("{} ({class}){tool} · {}", h.on, h.command);
            }
            std::process::ExitCode::SUCCESS
        }
        HooksAction::Test { event } => {
            let matching: Vec<_> = cfg
                .hooks
                .iter()
                .filter(|h| h.on == event)
                .filter(|h| ka_agent_core::config::AGENT_HOOK_EVENTS.contains(&h.on.as_str()))
                .collect();
            if matching.is_empty() {
                println!("no agent hooks for {event:?} (engine hooks run inside the engine)");
                return std::process::ExitCode::SUCCESS;
            }
            let envelope = serde_json::json!({
                "seq": 0,
                "at_ms": ka_agent_core::now_ms(),
                "ev": "notice",
                "text": format!("synthetic hook test: {event}"),
            });
            let mut failed = false;
            for h in matching {
                let mut cmd = if cfg!(windows) {
                    let mut c = std::process::Command::new("cmd");
                    c.args(["/C", &h.command]);
                    c
                } else {
                    let mut c = std::process::Command::new("sh");
                    c.args(["-c", &h.command]);
                    c
                };
                cmd.env("KA_HOOK_EVENT", &h.on)
                    .env("KA_HOOK_SUMMARY", format!("synthetic {event}"))
                    .stdin(std::process::Stdio::piped())
                    .stdout(std::process::Stdio::piped())
                    .stderr(std::process::Stdio::piped())
                    .current_dir(cfg.workspace_path());
                use std::io::Write as _;
                let mut child = match cmd.spawn() {
                    Ok(c) => c,
                    Err(e) => {
                        println!("{}: spawn failed: {e}", h.command);
                        failed = true;
                        continue;
                    }
                };
                if let Some(mut stdin) = child.stdin.take() {
                    let _ = stdin.write_all(envelope.to_string().as_bytes());
                }
                match child.wait_with_output() {
                    Ok(out) => {
                        println!(
                            "{}: exit {:?}{}{}",
                            h.command,
                            out.status.code(),
                            if out.stdout.is_empty() {
                                String::new()
                            } else {
                                format!(
                                    " · stdout: {}",
                                    String::from_utf8_lossy(&out.stdout).trim_end()
                                )
                            },
                            if out.stderr.is_empty() {
                                String::new()
                            } else {
                                format!(
                                    " · stderr: {}",
                                    String::from_utf8_lossy(&out.stderr).trim_end()
                                )
                            }
                        );
                        if !out.status.success() {
                            failed = true;
                        }
                    }
                    Err(e) => {
                        println!("{}: {e}", h.command);
                        failed = true;
                    }
                }
            }
            if failed {
                std::process::ExitCode::FAILURE
            } else {
                std::process::ExitCode::SUCCESS
            }
        }
    }
}
