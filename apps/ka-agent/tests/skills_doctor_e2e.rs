//! B12 e2e: skills frontmatter (`triggers`, `paths`, `allowed_tools`),
//! turn-scoped tool binding, and `/skill-doctor`, through the real
//! `ka-agent` binary with its scripted engine speaker.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
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

fn temp_root(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("ka-agent-skills-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn scenario_file(root: &Path, name: &str, body: &str) -> PathBuf {
    let p = root.join(name);
    std::fs::write(&p, body).unwrap();
    p
}

fn write_skill(cfg_home: &Path, name: &str, frontmatter: &str) {
    let dir = cfg_home.join("ka-agent/skills").join(name);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("SKILL.md"),
        format!("---\n{frontmatter}\n---\n\nDo the thing.\n"),
    )
    .unwrap();
}

fn enable(root: &Path, cfg_home: &Path, name: &str) {
    let out = Command::new(env!("CARGO_BIN_EXE_ka-agent"))
        .args(["skills", "enable", name])
        .env("KA_AGENT_DATA_DIR", root.join("data"))
        .env("XDG_CONFIG_HOME", cfg_home)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "skills enable {name}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn spawn_chat(root: &Path, scenario: &Path, cfg_home: &Path, mode: &str, extra: &str) -> Chat {
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
        .env("XDG_CONFIG_HOME", cfg_home)
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

    fn transcript(&self) -> String {
        self.seen.borrow().clone()
    }

    fn mark(&self) -> usize {
        self.seen.borrow().len()
    }

    fn pump(&mut self) {
        while let Ok(line) = self.rx.try_recv() {
            self.seen.borrow_mut().push_str(&line);
            self.seen.borrow_mut().push('\n');
        }
    }

    fn expect(&mut self, needle: &str, timeout: Duration) {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            self.pump();
            if self.seen.borrow().contains(needle) {
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        panic!(
            "timed out waiting for {needle:?}; output so far:\n{}",
            self.seen.borrow()
        );
    }

    fn expect_from(&mut self, needle: &str, from: usize, timeout: Duration) {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            self.pump();
            if self.seen.borrow().len() > from && self.seen.borrow()[from..].contains(needle) {
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        panic!(
            "timed out waiting for fresh {needle:?}; output so far:\n{}",
            self.seen.borrow()
        );
    }
}

fn line_with<'a>(transcript: &'a str, prefix: &str) -> &'a str {
    transcript
        .lines()
        .find(|line| line.trim_start().starts_with(prefix))
        .unwrap_or_else(|| panic!("no {prefix:?} line in:\n{transcript}"))
}

/// A whole-word trigger fires a plain message; `/skill-doctor` counts it;
/// an unrelated message changes nothing.
#[test]
fn skill_triggers_attach_on_mention() {
    let root = temp_root("trigger");
    let cfg_home = root.join("cfg-home");
    write_skill(
        &cfg_home,
        "deploy",
        "name = \"deploy\"\ndescription = \"ship the thing\"\ntriggers = [\"deploy\"]",
    );
    enable(&root, &cfg_home, "deploy");
    let sc = scenario_file(
        &root,
        "trigger.json",
        r#"{ "steps": [ {"text": "deploy-ack"} ], "stop": "done", "cost_usd": 0.0, "tokens": 1 }"#,
    );
    let mut chat = spawn_chat(&root, &sc, &cfg_home, "guarded", "");
    chat.expect("local session", Duration::from_secs(60));

    chat.send("how do we deploy this?");
    chat.expect("deploy-ack", Duration::from_secs(30));
    chat.expect("turn done", Duration::from_secs(10));

    chat.send("/skill-doctor");
    chat.expect("1× (trigger)", Duration::from_secs(10));

    // No trigger in this message: the count must not move. (The scripted
    // speaker's steps are spent, so the turn itself ends empty and the
    // runtime's bounded re-ask follows; the submit happened either way.)
    chat.send("unrelated chatter");
    chat.expect("asking again", Duration::from_secs(30));
    std::thread::sleep(Duration::from_millis(500));
    chat.send("/skill-doctor");
    let mark = chat.mark();
    chat.expect_from("1× (trigger)", mark, Duration::from_secs(10));
    chat.send("/quit");
    std::fs::remove_dir_all(&root).ok();
}

/// A `paths:` touch parks; the next turn attaches once; a later touch does
/// not re-attach (the attach-time marker).
#[test]
fn skill_paths_attach_once_on_touch() {
    let root = temp_root("paths");
    let cfg_home = root.join("cfg-home");
    write_skill(
        &cfg_home,
        "release-notes",
        "name = \"release-notes\"\ndescription = \"read the release notes\"\npaths = [\"docs/*\"]",
    );
    enable(&root, &cfg_home, "release-notes");
    let ws = root.join("ws");
    std::fs::create_dir_all(ws.join("docs")).unwrap();
    std::fs::write(ws.join("docs/release.md"), "v1 notes\n").unwrap();
    // The scripted speaker replays on every fresh worker, so each turn can
    // read the same path again. `bash` (not `read`) because the pinned
    // engine's `CallStarted.detail` for `read` is only the file name; the
    // paths matcher needs the `docs/` part the command carries.
    let sc = scenario_file(
        &root,
        "paths.json",
        r#"{ "steps": [
                {"call": {"tool": "bash", "args": {"command": "cat docs/release.md"}}},
                {"text": "paths-ack"}
             ], "stop": "done", "cost_usd": 0.0, "tokens": 1 }"#,
    );
    let mut chat = spawn_chat(
        &root,
        &sc,
        &cfg_home,
        "allow",
        "worker_idle_timeout_secs = 2\n",
    );
    chat.expect("local session", Duration::from_secs(60));

    // Turn 1: touch parks the skill; nothing attaches mid-cycle.
    chat.send("read docs/release.md");
    chat.expect("paths-ack", Duration::from_secs(30));
    chat.expect("turn done", Duration::from_secs(10));
    chat.expect("worker stopped", Duration::from_secs(30));

    // Turn 2: a fresh worker replays the same call; the parked skill
    // attaches at submit, so the marker suppresses the touch inside this
    // same turn.
    let mark = chat.mark();
    chat.send("again");
    chat.expect_from("paths-ack", mark, Duration::from_secs(30));
    chat.expect("turn done", Duration::from_secs(10));

    chat.send("/skill-doctor");
    chat.expect("1× (paths)", Duration::from_secs(10));
    chat.send("/quit");
    std::fs::remove_dir_all(&root).ok();
}

/// `/skill-doctor` prices the advertised line + body, echoes firing
/// metadata, and marks a provisioned copy whose source vanished.
#[test]
fn skill_doctor_prices_context() {
    let root = temp_root("doctor");
    let cfg_home = root.join("cfg-home");
    write_skill(
        &cfg_home,
        "alpha",
        "name = \"alpha\"\ndescription = \"plain skill\"",
    );
    write_skill(
        &cfg_home,
        "beta",
        "name = \"beta\"\ndescription = \"triggered skill\"\ntriggers = [\"beta\"]\npaths = [\"docs/*\"]",
    );
    write_skill(
        &cfg_home,
        "ghost",
        "name = \"ghost\"\ndescription = \"no source left\"",
    );
    for name in ["alpha", "beta", "ghost"] {
        enable(&root, &cfg_home, name);
    }
    // Drop ghost's operator copy (the `[enabled, no source copy]` restore
    // case); the engine copy stays.
    std::fs::remove_dir_all(cfg_home.join("ka-agent/skills/ghost")).unwrap();
    let sc = scenario_file(&root, "doctor.json", r#"{ "steps": [] }"#);
    let mut chat = spawn_chat(&root, &sc, &cfg_home, "guarded", "");
    chat.expect("local session", Duration::from_secs(60));

    chat.send("/skill-doctor");
    chat.expect("tok advertised · ", Duration::from_secs(10));
    chat.expect("standing:", Duration::from_secs(10));
    chat.expect("triggers: beta", Duration::from_secs(10));
    chat.expect("paths: docs/*", Duration::from_secs(10));
    chat.expect("no source copy", Duration::from_secs(10));
    let transcript = chat.transcript();
    assert!(
        transcript.contains("KiB body"),
        "body size is priced:\n{transcript}"
    );
    let alpha = line_with(&transcript, "alpha ");
    assert!(
        !alpha.contains("triggers:"),
        "triggerless skills show no trigger echo:\n{alpha}"
    );
    let ghost = line_with(&transcript, "ghost ");
    assert!(
        ghost.contains("no source copy"),
        "the no-source skill is marked:\n{ghost}"
    );
    chat.send("/quit");
    std::fs::remove_dir_all(&root).ok();
}
