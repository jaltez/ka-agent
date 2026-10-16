//! M8 e2e: the plugin lifecycle and the sandbox, through the real binary.
//!
//! - `ka-agent plugin keygen|pack|install|enable` on the probe plugin
//!   (examples/plugins/probe), then the bridge (`ka-agent __mcp`) must
//!   advertise and execute its tools.
//! - Sandbox escape assertions: the probe reports what it could touch; the
//!   test asserts every confinement line.
//! - Tampered installed files disable the plugin; an unsigned archive is
//!   refused.
//!
//! Requires bubblewrap (`apt install bubblewrap`); CI installs it.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};

fn temp_root(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("ka-plugin-e2e-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn ka(root: &std::path::Path) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_ka-agent"));
    cmd.env("KA_AGENT_DATA_DIR", root.join("data"));
    cmd
}

/// The compiled probe plugin binary (a workspace member, built by
/// `cargo test --workspace`).
fn probe_bin() -> std::path::PathBuf {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/debug/probe");
    assert!(
        path.exists(),
        "probe binary missing at {}: build the workspace first",
        path.display()
    );
    path
}

/// The probe plugin source directory, with its binary next to manifest.toml.
fn plugin_dir(root: &std::path::Path) -> std::path::PathBuf {
    let dir = root.join("probe-src");
    std::fs::create_dir_all(dir.join("bin")).unwrap();
    std::fs::write(
        dir.join("manifest.toml"),
        r#"
[plugin]
id = "probe"
version = "0.1.0"
api = 1
description = "sandbox probe"
entry = { type = "mcp-stdio", command = "/plugin/bin/probe" }

[requires]
secrets = ["probe.token"]

[[tools]]
name = "probe.sandbox"
effect = "read"

[[tools]]
name = "probe.env"
effect = "read"

[[tools]]
name = "probe.echo"
effect = "read"
"#,
    )
    .unwrap();
    std::fs::copy(probe_bin(), dir.join("bin/probe")).unwrap();
    dir
}

/// One MCP exchange over the bridge's stdio.
fn bridge_tools_list(data: &std::path::Path) -> (Vec<String>, String, String) {
    let mut child = Command::new(env!("CARGO_BIN_EXE_ka-agent"))
        .env("KA_AGENT_DATA_DIR", data)
        .arg("__mcp")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let stdin = child.stdin.as_mut().unwrap();
    stdin
        .write_all(
            br#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"test"}}}"#,
        )
        .unwrap();
    stdin.write_all(b"\n").unwrap();
    stdin
        .write_all(br#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#)
        .unwrap();
    stdin.write_all(b"\n").unwrap();
    stdin
        .write_all(br#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#)
        .unwrap();
    stdin.write_all(b"\n").unwrap();
    stdin.flush().unwrap();
    drop(child.stdin.take());
    let stdout = child.stdout.take().unwrap();
    let reader = BufReader::new(stdout);
    let mut names = Vec::new();
    let mut stderr_text = String::new();
    let mut list_seen = false;
    for line in reader.lines() {
        let Ok(line) = line else { break };
        let Ok(msg) = serde_json::from_str::<serde_json::Value>(&line) else {
            continue;
        };
        if msg.get("id").and_then(|i| i.as_i64()) == Some(2) {
            for tool in msg["result"]["tools"].as_array().unwrap_or(&vec![]) {
                if let Some(name) = tool["name"].as_str() {
                    names.push(name.to_string());
                }
            }
            list_seen = true;
            break;
        }
    }
    let status = child.wait().unwrap();
    use std::io::Read;
    let _ = status;
    if let Some(mut err) = child.stderr.take() {
        let _ = err.read_to_string(&mut stderr_text);
    }
    assert!(list_seen, "tools/list never answered");
    (names, stderr_text, String::new())
}

/// `tools/call` over the bridge's stdio; returns the answer text.
fn bridge_call(data: &std::path::Path, tool: &str, args: &str) -> String {
    let mut child = Command::new(env!("CARGO_BIN_EXE_ka-agent"))
        .env("KA_AGENT_DATA_DIR", data)
        .arg("__mcp")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let stdin = child.stdin.as_mut().unwrap();
    for msg in [
        r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"test"}}}"#,
        r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
        &format!(
            r#"{{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{{"name":"{tool}","arguments":{args}}}}}"#
        ),
    ] {
        stdin.write_all(msg.as_bytes()).unwrap();
        stdin.write_all(b"\n").unwrap();
    }
    stdin.flush().unwrap();
    drop(child.stdin.take());
    let stdout = child.stdout.take().unwrap();
    for line in BufReader::new(stdout).lines() {
        let Ok(line) = line else { break };
        let Ok(msg) = serde_json::from_str::<serde_json::Value>(&line) else {
            continue;
        };
        if msg.get("id").and_then(|i| i.as_i64()) == Some(2) {
            let text = msg["result"]["content"][0]["text"]
                .as_str()
                .unwrap_or_default()
                .to_string();
            let _ = child.kill();
            let _ = child.wait();
            return text;
        }
    }
    let _ = child.kill();
    let _ = child.wait();
    panic!("{tool}: the bridge never answered");
}

#[test]
fn plugin_lifecycle_sandbox_and_integrity() {
    if !std::process::Command::new("bwrap")
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok()
    {
        panic!("bubblewrap is required for the plugin sandbox tests: apt install bubblewrap");
    }
    let root = temp_root("lifecycle");
    let data = root.join("data");

    // keygen → pack → install → enable (all through the real CLI).
    let run = |args: &[&str]| {
        let out = ka(&root).args(args).output().unwrap();
        (
            out.status,
            String::from_utf8_lossy(&out.stdout).into_owned(),
            String::from_utf8_lossy(&out.stderr).into_owned(),
        )
    };
    let (status, _, stderr) = run(&["plugin", "keygen"]);
    assert!(status.success(), "keygen: {stderr}");
    let dir = plugin_dir(&root);
    let archive = root.join("probe-0.1.0.kplug");
    let (status, stdout, stderr) = run(&[
        "plugin",
        "pack",
        dir.to_str().unwrap(),
        "--out",
        archive.to_str().unwrap(),
    ]);
    assert!(status.success(), "pack: {stderr} {stdout}");
    assert!(
        archive.exists(),
        "archive written next to the source: {stdout}"
    );
    let (status, stdout, stderr) = run(&["plugin", "install", archive.to_str().unwrap()]);
    assert!(status.success(), "install: {stderr} {stdout}");
    assert!(
        stdout.contains("still disabled"),
        "install never enables: {stdout}"
    );
    let (status, stdout, _) = run(&["plugin", "enable", "probe"]);
    assert!(status.success(), "enable: {stdout}");

    // The declared secret (SEED §6.4: a plugin sees only what it declares).
    let secrets = data.join("secrets");
    std::fs::create_dir_all(&secrets).unwrap();
    std::fs::write(secrets.join("probe.token"), "tok-123").unwrap();

    // The bridge must now advertise the plugin's tools.
    let (names, stderr, _) = bridge_tools_list(&data);
    let lock_debug =
        std::fs::read_to_string(data.join("plugins/installed.json")).unwrap_or_default();
    for expected in ["probe.sandbox", "probe.env", "probe.echo"] {
        assert!(
            names.iter().any(|n| n == expected),
            "tools/list missing {expected}: {names:?}\nbridge stderr: {stderr}\nlockfile: {lock_debug}"
        );
    }
    assert!(
        names.contains(&"memory_propose".to_string()),
        "first-party tools stay: {names:?}"
    );

    // Sandbox escape assertions, straight from the plugin's report.
    let report = bridge_call(&data, "probe.sandbox", "{}");
    for expected in [
        "tmp_write: ok",
        "plugin_write: blocked",
        "etc_passwd: blocked",
        "net_connect: blocked",
        "home: /tmp/home",
        "secret: ok",
        "host_env_leak: blocked",
    ] {
        assert!(
            report.contains(expected),
            "sandbox report missing {expected:?}:\n{report}"
        );
    }
    assert!(
        !report.contains("FAILED"),
        "a confinement check failed:\n{report}"
    );

    // Tamper with the installed binary: the bridge must disable the plugin.
    let installed = data.join("plugins/probe/bin/probe");
    std::fs::write(&installed, b"not a real binary anymore").unwrap();
    let (names, stderr, _) = bridge_tools_list(&data);
    assert!(
        !names.iter().any(|n| n == "probe.sandbox"),
        "a tampered plugin must not be offered: {names:?}"
    );
    assert!(
        stderr.contains("probe disabled"),
        "the refusal must be visible: {stderr}"
    );

    // Reinstall the untampered plugin (same key → TOFU allows the update),
    // then confirm it is offered again.
    let (status, _, stderr) = run(&["plugin", "install", archive.to_str().unwrap()]);
    assert!(status.success(), "reinstall: {stderr}");
    let (names, _, _) = bridge_tools_list(&data);
    assert!(
        names.iter().any(|n| n == "probe.sandbox"),
        "reinstalled plugin must be offered again: {names:?}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn unsigned_and_disabled_are_refused() {
    let root = temp_root("refused");
    let dir = plugin_dir(&root);
    let archive = root.join("probe-0.1.0.kplug");

    // No keygen: no trusted keys → install refuses and writes nothing.
    let out = ka(&root)
        .args(["plugin", "install", archive.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(
        !out.status.success(),
        "install without trusted keys must fail"
    );
    assert!(
        !root.join("data/plugins/probe").exists(),
        "a refused install must not unpack"
    );

    // A real install+enable, then disable: the bridge stops offering it.
    let keygen = ka(&root).args(["plugin", "keygen"]).output().unwrap();
    assert!(keygen.status.success());
    let pack = ka(&root)
        .args([
            "plugin",
            "pack",
            dir.to_str().unwrap(),
            "--out",
            archive.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(
        pack.status.success(),
        "{}",
        String::from_utf8_lossy(&pack.stderr)
    );
    let install = ka(&root)
        .args(["plugin", "install", archive.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(install.status.success());
    let disable = ka(&root)
        .args(["plugin", "disable", "probe"])
        .output()
        .unwrap();
    assert!(disable.status.success());
    let (names, _, _) = bridge_tools_list(&root.join("data"));
    assert!(
        !names.iter().any(|n| n == "probe.sandbox"),
        "a disabled plugin must not be offered: {names:?}"
    );
    // …and the CLI still lists it, disabled.
    let list = ka(&root).args(["plugin", "list"]).output().unwrap();
    let text = String::from_utf8_lossy(&list.stdout);
    assert!(text.contains("probe"), "list: {text}");
    assert!(text.contains("disabled"), "list: {text}");
    let _ = std::fs::remove_dir_all(&dir);
}
