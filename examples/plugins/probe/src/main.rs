//! The sandbox probe: the M8 example plugin and the sandbox-escape test
//! subject (`docs/contract/plugins.md`). A minimal MCP-stdio server in
//! dependency-free Rust — three read tools:
//!
//! - `probe.sandbox` — what the sandbox sees and refuses (writes, reads,
//!   network): the escape test reads this report and asserts every line.
//! - `probe.env` — the process environment (PATH/HOME, no host env leaks).
//! - `probe.echo` — returns `args.text` unchanged.
//!
//! Wire: newline-delimited JSON-RPC 2.0 over stdio (MCP 2025-06-18).

use std::io::{BufRead, Write};

fn main() {
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        let Ok(msg) = serde_json::from_str::<serde_json::Value>(&line) else {
            continue;
        };
        let id = msg.get("id").cloned();
        let Some(method) = msg.get("method").and_then(|m| m.as_str()) else {
            continue;
        };
        let response = match (method, msg["params"]["name"].as_str()) {
            ("initialize", _) => serde_json::json!({
                "protocolVersion": "2025-06-18",
                "capabilities": {"tools": {}},
                "serverInfo": {"name": "probe", "version": "0.1.0"}
            }),
            ("tools/call", Some("probe.sandbox")) => {
                let report = sandbox_report();
                serde_json::json!({"content": [{"type": "text", "text": report}]})
            }
            ("tools/call", Some("probe.env")) => {
                let report = env_report();
                serde_json::json!({"content": [{"type": "text", "text": report}]})
            }
            ("tools/call", Some("probe.mark")) => {
                serde_json::json!({"content": [{"type": "text", "text": "marked"}]})
            }
            ("tools/call", Some("probe.echo")) => {
                let text = msg["params"]["arguments"]["text"].as_str().unwrap_or("");
                serde_json::json!({"content": [{"type": "text", "text": text}]})
            }
            ("tools/call", _) => serde_json::json!({
                "content": [{"type": "text", "text": "unknown tool"}],
                "isError": true
            }),
            _ => continue,
        };
        let _ = writeln!(
            stdout,
            "{}",
            serde_json::json!({"jsonrpc": "2.0", "id": id, "result": response})
        );
        let _ = stdout.flush();
    }
}

/// Every check the escape test asserts, one `name: value` line each.
/// `expected` below is what a correctly confined sandbox produces:
/// writes land only in /tmp, system files are absent, the network is gone,
/// and only the declared secret is visible.
fn sandbox_report() -> String {
    let mut lines = Vec::new();

    // 1. /tmp is a private tmpfs: writable.
    let tmp_write = std::fs::write("/tmp/probe-marker", b"x").is_ok();
    lines.push(format!("tmp_write: {}", ok(tmp_write)));

    // 2. /plugin is mounted read-only: escaping the workspace must fail.
    let plugin_write = std::fs::write("/plugin/escape", b"x").is_ok();
    lines.push(format!("plugin_write: {}", blocked(!plugin_write)));

    // 3. Outside the mounts (/etc) is not present at all.
    let etc = std::fs::read_to_string("/etc/passwd").is_ok();
    lines.push(format!("etc_passwd: {}", blocked(!etc)));

    // 4. No network: the namespace is unshared, connect() must fail.
    let net = std::net::TcpStream::connect_timeout(
        &"127.0.0.1:9".parse().expect("parse addr"),
        std::time::Duration::from_millis(500),
    )
    .is_ok();
    lines.push(format!("net_connect: {}", blocked(!net)));

    // 5. Only the declared secret is visible, never other env.
    lines.push(format!(
        "home: {}",
        std::env::var("HOME").unwrap_or_else(|_| "<unset>".into())
    ));
    lines.push(format!(
        "secret: {}",
        ok(std::env::var("PROBE_TOKEN").as_deref() == Ok("tok-123"))
    ));
    lines.push(format!(
        "host_env_leak: {}",
        blocked(std::env::var_os("KA_AGENT_GITHUB_TOKEN").is_none())
    ));
    lines.join("\n")
}

/// The process environment the sandbox defines: PATH and HOME only.
fn env_report() -> String {
    let mut names: Vec<String> = std::env::vars_os()
        .map(|(k, _)| k.to_string_lossy().into_owned())
        .collect();
    names.sort();
    format!("env vars: {}", names.join(", "))
}

fn ok(yes: bool) -> &'static str {
    if yes {
        "ok"
    } else {
        "FAILED"
    }
}

fn blocked(yes: bool) -> &'static str {
    if yes {
        "blocked"
    } else {
        "FAILED"
    }
}
