//! Grants e2e: path grants, the install gate and `grants.full`, driven
//! through the real binary + scripted scenarios (the chat_e2e pattern).
//! Every assertion counts approval envelopes on the admin attach stream,
//! so "no approval" is the absence of an `"ev":"approval"` line, not a
//! timeout.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{Receiver, channel};
use std::time::{Duration, Instant};

struct Chat {
    child: Child,
    rx: Receiver<String>,
    seen: std::cell::RefCell<String>,
}

impl Drop for Chat {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// The admin attach stream: every envelope as one JSON line.
struct Tap {
    rx: Receiver<String>,
    seen: std::cell::RefCell<String>,
}

impl Tap {
    fn connect(root: &std::path::Path) -> Self {
        let sock = root.join("data/admin.sock");
        let deadline = Instant::now() + Duration::from_secs(20);
        while !sock.exists() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(sock.exists(), "local chat did not bind the admin socket");
        let mut stream = std::os::unix::net::UnixStream::connect(&sock).unwrap();
        stream
            .write_all(b"{\"op\":\"attach\",\"since\":0}\n")
            .unwrap();
        stream.flush().unwrap();
        let (tx, rx) = channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stream).lines() {
                let Ok(line) = line else { break };
                if tx.send(line).is_err() {
                    break;
                }
            }
        });
        Tap {
            rx,
            seen: std::cell::RefCell::new(String::new()),
        }
    }

    fn drain(&self) {
        while let Ok(line) = self.rx.try_recv() {
            self.seen.borrow_mut().push_str(&line);
            self.seen.borrow_mut().push('\n');
        }
    }

    /// Wait until output appended after `from` contains `needle`.
    fn expect_from(&self, needle: &str, from: usize, timeout: Duration) {
        let deadline = Instant::now() + timeout;
        loop {
            self.drain();
            let hit = {
                let seen = self.seen.borrow();
                seen.len() > from && seen[from..].contains(needle)
            };
            if hit {
                return;
            }
            if Instant::now() >= deadline {
                panic!(
                    "timed out waiting for {needle:?}; attach stream:\n{}",
                    self.seen.borrow()
                );
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    fn mark(&self) -> usize {
        self.drain();
        self.seen.borrow().len()
    }

    /// Occurrences of `needle` appended after `from`.
    fn count_from(&self, needle: &str, from: usize) -> usize {
        self.drain();
        self.seen.borrow()[from..].matches(needle).count()
    }
}

/// Spawn `ka-agent chat` with an explicit mode and extra config lines.
fn spawn(root: &std::path::Path, scenario: &std::path::Path, mode: &str, extra: &str) -> Chat {
    let ws = root.join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let cfg = root.join("ka-agent.toml");
    std::fs::write(
        &cfg,
        format!(
            "workspace = {:?}\nmode = {:?}\napproval_ttl_secs = 300\nturn_grace_secs = 5\n{}",
            ws.display().to_string(),
            mode,
            extra
        ),
    )
    .unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_ka-agent"))
        .arg("chat")
        .arg("--config")
        .arg(&cfg)
        .arg("--scenario")
        .arg(scenario)
        .env("KA_AGENT_DATA_DIR", root.join("data"))
        .env_remove("KA_MODEL")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let rx = {
        let out = child.stdout.take().unwrap();
        let (tx, rx) = channel();
        std::thread::spawn(move || {
            for line in BufReader::new(out).lines() {
                let Ok(line) = line else { break };
                if tx.send(line).is_err() {
                    break;
                }
            }
        });
        rx
    };
    Chat {
        child,
        rx,
        seen: std::cell::RefCell::new(String::new()),
    }
}

impl Chat {
    fn send(&mut self, line: &str) {
        let stdin = self.child.stdin.as_mut().unwrap();
        stdin.write_all(format!("{line}\n").as_bytes()).unwrap();
        stdin.flush().unwrap();
    }

    /// Wait until stdout contains `needle`.
    fn expect(&mut self, needle: &str, timeout: Duration) {
        let deadline = Instant::now() + timeout;
        loop {
            while let Ok(line) = self.rx.try_recv() {
                self.seen.borrow_mut().push_str(&line);
                self.seen.borrow_mut().push('\n');
            }
            if self.seen.borrow().contains(needle) {
                return;
            }
            if Instant::now() >= deadline {
                panic!(
                    "timed out waiting for {needle:?}; output so far:\n{}",
                    self.seen.borrow()
                );
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    fn transcript(&self) -> String {
        while let Ok(line) = self.rx.try_recv() {
            self.seen.borrow_mut().push_str(&line);
            self.seen.borrow_mut().push('\n');
        }
        self.seen.borrow().clone()
    }
}

fn temp_root(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("ka-agent-grants-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn scenario_file(root: &std::path::Path, name: &str, body: &str) -> std::path::PathBuf {
    let p = root.join(name);
    std::fs::write(&p, body).unwrap();
    p
}

/// `ka-agent doctor --config <cfg> --json` output (with the instance's
/// data root pinned, matching the chat under test).
fn doctor_json(cfg: &std::path::Path, data_root: &std::path::Path) -> String {
    let out = Command::new(env!("CARGO_BIN_EXE_ka-agent"))
        .arg("doctor")
        .arg("--config")
        .arg(cfg)
        .arg("--json")
        .env("KA_AGENT_DATA_DIR", data_root)
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// Criterion 1 — `mode = "allow"`: bash in the workspace never asks, the turn
/// finishes and the tool output reaches the surface (a failed call's
/// excerpt is the only tool output surfaces see, so the scenario's
/// `echo hi` exits nonzero on purpose).
#[test]
fn allow_mode_bash_never_asks_and_outputs() {
    let root = temp_root("t1");
    let sc = scenario_file(
        &root,
        "t1.json",
        r#"{ "steps": [
            {"call": {"tool": "bash", "args": {"command": "echo hi; exit 1"}}},
            {"text": "ran it"}
        ], "stop": "done", "cost_usd": 0.0, "tokens": 1 }"#,
    );
    let mut chat = spawn(&root, &sc, "allow", "");
    let tap = Tap::connect(&root);
    let from = tap.mark();
    chat.send("go");
    // the bash call finished with its output, no approval ever appeared,
    // and the turn completed
    tap.expect_from("\"tool\":\"bash\"", from, Duration::from_secs(60));
    // the failed call's first output line is the pinned engine's
    // "(exit N)" prefix — the excerpt is the only tool output any
    // surface carries; the stdout itself rides the model's tool result
    tap.expect_from("\"error\":\"(exit 1)", from, Duration::from_secs(30));
    tap.expect_from("\"ev\":\"turn_finished\"", from, Duration::from_secs(30));
    assert_eq!(tap.count_from("\"ev\":\"approval\"", from), 0);
    chat.expect("ran it", Duration::from_secs(30));
    chat.send("/quit");
    std::fs::remove_dir_all(&root).ok();
}

/// Criterion 2 — `mode = "allow"`: a write outside the workspace runs with no
/// approval; a `deny` grant blocks bash with the grant named.
#[test]
fn allow_mode_outside_write_runs_and_deny_grant_blocks() {
    // 2a: outside the workspace, no grant: runs, no approval.
    let root = temp_root("t2a");
    let target = root.join("outside-grants");
    let sc = scenario_file(
        &root,
        "t2a.json",
        &format!(
            r#"{{ "steps": [
                {{"call": {{"tool": "bash", "args": {{"command": "touch {}"}}}}}},
                {{"text": "done"}}
            ], "stop": "done", "cost_usd": 0.0, "tokens": 1 }}"#,
            target.display()
        ),
    );
    let mut chat = spawn(&root, &sc, "allow", "");
    let tap = Tap::connect(&root);
    let from = tap.mark();
    chat.send("go");
    chat.expect("done", Duration::from_secs(60));
    tap.expect_from("\"ev\":\"turn_finished\"", from, Duration::from_secs(30));
    assert_eq!(tap.count_from("\"ev\":\"approval\"", from), 0);
    assert!(target.exists(), "the sandbox-free write must land");
    chat.send("/quit");
    std::fs::remove_dir_all(&root).ok();

    // 2b: the same touch inside a deny dir is blocked by the guard.
    let root = temp_root("t2b");
    let nope = root.join("nope");
    std::fs::create_dir_all(&nope).unwrap();
    let sc = scenario_file(
        &root,
        "t2b.json",
        &format!(
            r#"{{ "steps": [
                {{"call": {{"tool": "bash", "args": {{"command": "touch {}"}}}}}},
                {{"text": "blocked as expected"}}
            ], "stop": "done", "cost_usd": 0.0, "tokens": 1 }}"#,
            nope.join("x").display()
        ),
    );
    let extra = format!(
        "[[grants.path]]\ndir = {:?}\naccess = \"deny\"\n",
        nope.display().to_string()
    );
    let mut chat = spawn(&root, &sc, "allow", &extra);
    let tap = Tap::connect(&root);
    let from = tap.mark();
    chat.send("go");
    tap.expect_from("denied by grant", from, Duration::from_secs(60));
    chat.expect("blocked as expected", Duration::from_secs(30));
    assert_eq!(tap.count_from("\"ev\":\"approval\"", from), 0);
    assert!(!nope.join("x").exists(), "the deny grant must hold");
    chat.send("/quit");
    std::fs::remove_dir_all(&root).ok();
}

/// Criterion 3 — ask mode keeps today's approval for a probe at the data root;
/// allow mode blocks the same probe as ka-agent's own files.
#[test]
fn ask_asks_but_allow_blocks_own_data_root() {
    // 3a: ask mode raises the approval (sandbox/gate behavior today).
    let root = temp_root("t3a");
    let data = root.join("data");
    std::fs::create_dir_all(&data).unwrap();
    let sc = scenario_file(
        &root,
        "t3a.json",
        &format!(
            r#"{{ "steps": [
                {{"call": {{"tool": "bash", "args": {{"command": "touch {}"}}}}}},
                {{"text": "whatever you decided"}}
            ], "stop": "done", "cost_usd": 0.0, "tokens": 1 }}"#,
            data.join("state.db.probe").display()
        ),
    );
    let mut chat = spawn(&root, &sc, "ask", "");
    let tap = Tap::connect(&root);
    let from = tap.mark();
    chat.send("go");
    tap.expect_from("\"ev\":\"approval\"", from, Duration::from_secs(60));
    // Leave it unanswered: dropping the chat denies on cleanup.
    drop(chat);
    assert!(!data.join("state.db.probe").exists());
    std::fs::remove_dir_all(&root).ok();

    // 3b: allow mode — no gate remains, the guard protects the root.
    let root = temp_root("t3b");
    let data = root.join("data");
    std::fs::create_dir_all(&data).unwrap();
    let sc = scenario_file(
        &root,
        "t3b.json",
        &format!(
            r#"{{ "steps": [
                {{"call": {{"tool": "bash", "args": {{"command": "touch {}"}}}}}},
                {{"text": "protected then"}}
            ], "stop": "done", "cost_usd": 0.0, "tokens": 1 }}"#,
            data.join("state.db.probe").display()
        ),
    );
    let mut chat = spawn(&root, &sc, "allow", "");
    let tap = Tap::connect(&root);
    let from = tap.mark();
    chat.send("go");
    tap.expect_from("ka-agent's own files", from, Duration::from_secs(60));
    chat.expect("protected then", Duration::from_secs(30));
    assert_eq!(tap.count_from("\"ev\":\"approval\"", from), 0);
    assert!(!data.join("state.db.probe").exists());
    chat.send("/quit");
    std::fs::remove_dir_all(&root).ok();
}

/// Criterion 4 — allow grants write without asking inside their dirs
/// (TWO dirs: the guard pre-approves per path, so both behave the
/// same — the pin for the multi-dir case); outside them the write
/// still raises the approval.
#[test]
fn path_allow_grant_writes_without_asking() {
    let root = temp_root("t4a");
    let ok1 = root.join("ok1");
    let ok2 = root.join("ok2");
    std::fs::create_dir_all(ok1.join("sub")).unwrap();
    std::fs::create_dir_all(&ok2).unwrap();
    let sc = scenario_file(
        &root,
        "t4a.json",
        &format!(
            r#"{{ "steps": [
                {{"call": {{"tool": "write", "args": {{"path": "{}", "content": "granted"}}}}}},
                {{"call": {{"tool": "write", "args": {{"path": "{}", "content": "also granted"}}}}}},
                {{"text": "written"}}
            ], "stop": "done", "cost_usd": 0.0, "tokens": 1 }}"#,
            ok1.join("sub/x").display(),
            ok2.join("y").display()
        ),
    );
    let extra = format!(
        "[[grants.path]]\ndir = {:?}\naccess = \"allow\"\n\
         [[grants.path]]\ndir = {:?}\naccess = \"always\"\n",
        ok1.display().to_string(),
        ok2.display().to_string()
    );
    let mut chat = spawn(&root, &sc, "ask", &extra);
    let tap = Tap::connect(&root);
    let from = tap.mark();
    chat.send("go");
    chat.expect("written", Duration::from_secs(60));
    tap.expect_from("\"ev\":\"turn_finished\"", from, Duration::from_secs(30));
    assert_eq!(tap.count_from("\"ev\":\"approval\"", from), 0);
    assert_eq!(
        std::fs::read_to_string(ok1.join("sub/x")).unwrap_or_default(),
        "granted"
    );
    assert_eq!(
        std::fs::read_to_string(ok2.join("y")).unwrap_or_default(),
        "also granted"
    );
    chat.send("/quit");
    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn ungranted_write_outside_the_grant_still_asks() {
    let root = temp_root("t4b");
    let ok = root.join("ok");
    let other = root.join("other");
    std::fs::create_dir_all(&ok).unwrap();
    std::fs::create_dir_all(&other).unwrap();
    let sc = scenario_file(
        &root,
        "t4b.json",
        &format!(
            r#"{{ "steps": [
                {{"call": {{"tool": "write", "args": {{"path": "{}", "content": "nope"}}}}}},
                {{"text": "awaiting you"}}
            ], "stop": "done", "cost_usd": 0.0, "tokens": 1 }}"#,
            other.join("x").display()
        ),
    );
    let extra = format!(
        "[[grants.path]]\ndir = {:?}\naccess = \"allow\"\n",
        ok.display().to_string()
    );
    let mut chat = spawn(&root, &sc, "ask", &extra);
    let tap = Tap::connect(&root);
    let from = tap.mark();
    chat.send("go");
    tap.expect_from("\"ev\":\"approval\"", from, Duration::from_secs(60));
    assert!(!other.join("x").exists());
    drop(chat);
    std::fs::remove_dir_all(&root).ok();
}

/// Criterion 5 — the install gate: denied with the switch named; pre-approved
/// (and actually attempted) with `grants.install = true`. The allowed
/// half pins `sandbox = "off"`: the pinned engine appends its own
/// network-grant ask for install-class commands even under a hook
/// pre-approve, so the sandbox would ask on its own.
#[test]
fn install_gate_denies_then_preapproves() {
    // 5a: default config, ask mode: the guard names the switch.
    let root = temp_root("t5a");
    let sc = scenario_file(
        &root,
        "t5a.json",
        r#"{ "steps": [
            {"call": {"tool": "bash", "args": {"command": "sudo apt-get install -y cowsay"}}},
            {"text": "denied as configured"}
        ], "stop": "done", "cost_usd": 0.0, "tokens": 1 }"#,
    );
    let mut chat = spawn(&root, &sc, "ask", "");
    let tap = Tap::connect(&root);
    let from = tap.mark();
    chat.send("go");
    tap.expect_from("grants.install = true", from, Duration::from_secs(60));
    chat.expect("denied as configured", Duration::from_secs(30));
    assert_eq!(tap.count_from("\"ev\":\"approval\"", from), 0);
    chat.send("/quit");
    std::fs::remove_dir_all(&root).ok();

    // 5b: grant on: no approval and the command runs (a nonexistent
    // package keeps the attempt harmless wherever sudo exists).
    let root = temp_root("t5b");
    let sc = scenario_file(
        &root,
        "t5b.json",
        r#"{ "steps": [
            {"call": {"tool": "bash", "args": {"command": "sudo apt-get install -y ka-agent-e2e-no-such-package"}}},
            {"text": "attempted"}
        ], "stop": "done", "cost_usd": 0.0, "tokens": 1 }"#,
    );
    let mut chat = spawn(
        &root,
        &sc,
        "ask",
        "sandbox = \"off\"\n[grants]\ninstall = true\n",
    );
    let tap = Tap::connect(&root);
    let from = tap.mark();
    chat.send("go");
    chat.expect("attempted", Duration::from_secs(60));
    tap.expect_from("\"ev\":\"turn_finished\"", from, Duration::from_secs(30));
    assert_eq!(tap.count_from("\"ev\":\"approval\"", from), 0);
    // the failure text is the shell's own, never the grant's
    let bash_errors: Vec<String> = tap
        .seen
        .borrow()
        .lines()
        .filter(|l| l.contains("\"ev\":\"tool_finished\"") && l.contains("\"tool\":\"bash\""))
        .map(str::to_string)
        .collect();
    assert!(
        bash_errors.iter().all(|l| !l.contains("grant")),
        "shell's own error expected: {bash_errors:?}"
    );
    chat.send("/quit");
    std::fs::remove_dir_all(&root).ok();
}

/// Criterion 6 — `grants.full`: nothing asks, the advisory shows in status and
/// doctor, and `advisory = false` silences both without changing
/// access.
#[test]
fn full_access_never_asks_and_advises() {
    let root = temp_root("t6a");
    let any = root.join("any");
    let out_ws = root.join("out-ws");
    let sc = scenario_file(
        &root,
        "t6a.json",
        &format!(
            r#"{{ "steps": [
                {{"call": {{"tool": "bash", "args": {{"command": "touch {}"}}}}}},
                {{"call": {{"tool": "write", "args": {{"path": "{}", "content": "full"}}}}}},
                {{"text": "all done"}}
            ], "stop": "done", "cost_usd": 0.0, "tokens": 1 }}"#,
            any.display(),
            out_ws.join("x").display()
        ),
    );
    let mut chat = spawn(&root, &sc, "ask", "[grants]\nfull = true\n");
    let tap = Tap::connect(&root);
    let from = tap.mark();
    chat.send("go");
    chat.expect("all done", Duration::from_secs(60));
    tap.expect_from("\"ev\":\"turn_finished\"", from, Duration::from_secs(30));
    assert_eq!(tap.count_from("\"ev\":\"approval\"", from), 0);
    assert!(any.exists());
    assert!(out_ws.join("x").exists());
    // the advisory rides /status and doctor --json
    chat.send("/status");
    chat.expect(
        "⚠ full access (grants.full): nothing asks",
        Duration::from_secs(30),
    );
    let cfg = root.join("ka-agent.toml");
    let doc = doctor_json(&cfg, &root.join("data"));
    assert!(doc.contains("grants.full is on"), "{doc}");
    assert!(doc.contains("grants: 0 paths, install on"), "{doc}");
    chat.send("/quit");
    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn full_access_advisory_off_silences_status_and_doctor() {
    let root = temp_root("t6b");
    let any = root.join("any");
    let sc = scenario_file(
        &root,
        "t6b.json",
        &format!(
            r#"{{ "steps": [
                {{"call": {{"tool": "bash", "args": {{"command": "touch {}"}}}}}},
                {{"text": "still full"}}
            ], "stop": "done", "cost_usd": 0.0, "tokens": 1 }}"#,
            any.display()
        ),
    );
    let mut chat = spawn(
        &root,
        &sc,
        "ask",
        "[grants]\nfull = true\nadvisory = false\n",
    );
    let tap = Tap::connect(&root);
    let from = tap.mark();
    chat.send("go");
    chat.expect("still full", Duration::from_secs(60));
    tap.expect_from("\"ev\":\"turn_finished\"", from, Duration::from_secs(30));
    assert_eq!(tap.count_from("\"ev\":\"approval\"", from), 0);
    assert!(any.exists());
    chat.send("/status");
    chat.expect("mode ask", Duration::from_secs(30));
    let out = chat.transcript();
    assert!(!out.contains("full access"), "{out}");
    let cfg = root.join("ka-agent.toml");
    let doc = doctor_json(&cfg, &root.join("data"));
    assert!(!doc.contains("grants.full is on"), "{doc}");
    assert!(doc.contains("grants: 0 paths"), "{doc}");
    chat.send("/quit");
    std::fs::remove_dir_all(&root).ok();
}
