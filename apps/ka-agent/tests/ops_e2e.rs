//! Ops e2e (SEED.md §13.10): `doctor` reports versions and instance state,
//! `ka-agent restart` stops the daemon gracefully over the admin socket,
//! and `ka-agent backup` → `ka-agent restore` survives a drill — the
//! restored instance opens, keeps its memory, history, grants and
//! identities, and still runs an engine turn.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

fn temp_root(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("ka-agent-ops-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn write_config(root: &Path) -> PathBuf {
    let ws = root.join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let cfg = root.join("ka-agent.toml");
    std::fs::write(
        &cfg,
        format!(
            "workspace = {:?}\nmode = \"guarded\"\nturn_grace_secs = 5\n",
            ws.display().to_string()
        ),
    )
    .unwrap();
    cfg
}

struct Daemon {
    child: Child,
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn wait_admin_socket(data: &Path, timeout: Duration) -> PathBuf {
    let sock = data.join("admin.sock");
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if sock.exists() {
            return sock;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    panic!("admin socket never appeared at {}", sock.display());
}

fn ka(data: &Path) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_ka-agent"));
    cmd.env("KA_AGENT_DATA_DIR", data);
    cmd
}

#[test]
fn doctor_reports_pin_contract_state_and_backup() {
    let root = temp_root("doctor");
    let cfg = write_config(&root);
    let data = root.join("data");

    // Known state: one linked identity.
    let out = ka(&data)
        .args(["identity", "link", "telegram", "12345"])
        .output()
        .unwrap();
    assert!(out.status.success());

    // No backup yet → the honest line.
    let out = ka(&data)
        .arg("doctor")
        .arg("--config")
        .arg(&cfg)
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("ka pin : rev a56aae2"), "pin line: {text}");
    assert!(
        text.contains("contract suite: ") && text.contains("(10 documents)"),
        "contract hash: {text}"
    );
    assert!(text.contains("identities: 1 linked"), "{text}");
    assert!(text.contains("backup : none yet"), "{text}");
    let hash1: String = text
        .lines()
        .find(|l| l.starts_with("contract suite: "))
        .unwrap()
        .chars()
        .skip("contract suite: ".len())
        .take(16)
        .collect();

    // After a backup, doctor points at it.
    let backup_dir = root.join("bk");
    let out = ka(&data)
        .arg("backup")
        .arg(&backup_dir)
        .arg("--config")
        .arg(&cfg)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "backup: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(backup_dir.join("state.db").exists());
    assert!(backup_dir.join("manifest.json").exists());
    let out = ka(&data)
        .arg("doctor")
        .arg("--config")
        .arg(&cfg)
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("backup : latest"), "{text}");
    // The hash is stable across runs (same embedded contract).
    let hash2: String = text
        .lines()
        .find(|l| l.starts_with("contract suite: "))
        .unwrap()
        .chars()
        .skip("contract suite: ".len())
        .take(16)
        .collect();
    assert_eq!(hash1, hash2, "contract hash must be deterministic");

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn restart_stops_the_daemon_gracefully() {
    let root = temp_root("restart");
    let cfg = write_config(&root);
    let data = root.join("data");
    let mut daemon = Daemon {
        child: ka(&data)
            .args(["run", "--config", cfg.to_str().unwrap()])
            .env_remove("KA_MODEL")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    };
    wait_admin_socket(&data, Duration::from_secs(15));

    let out = ka(&data)
        .arg("restart")
        .arg("--config")
        .arg(&cfg)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "restart: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("shutting_down"), "{stdout}");

    // The daemon exits 0 (graceful), promptly.
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        match daemon.child.try_wait().unwrap() {
            Some(code) => {
                assert!(code.success(), "daemon exit was {code:?}");
                break;
            }
            None if Instant::now() > deadline => {
                panic!("daemon did not exit after restart")
            }
            None => std::thread::sleep(Duration::from_millis(50)),
        }
    }
    let _ = std::fs::remove_dir_all(&root);
}

/// Drive one chat instance: a scripted turn, a `/remember`, then quit.
fn drive_instance(root: &Path, marker: &str) {
    let cfg = write_config(root);
    let sc = root.join("s.json");
    std::fs::write(
        &sc,
        r#"{ "steps": [ {"text": "the answer is forty-two"} ], "stop": "done", "cost_usd": 0.0, "tokens": 3 }"#,
    )
    .unwrap();
    let mut child = ka(&root.join("data"))
        .args([
            "chat",
            "--config",
            cfg.to_str().unwrap(),
            "--scenario",
            sc.to_str().unwrap(),
        ])
        .env_remove("KA_MODEL")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let stdout = child.stdout.take().unwrap();
    let (tx, rx) = std::sync::mpsc::channel::<String>();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            if tx.send(line).is_err() {
                break;
            }
        }
    });
    let mut seen = String::new();
    let stdin = child.stdin.as_mut().unwrap();
    let _ = writeln!(stdin, "meaning of life?");
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        while let Ok(line) = rx.try_recv() {
            seen.push_str(&line);
            seen.push('\n');
        }
        if seen.contains("forty-two") {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "instance never answered:\n{seen}"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    let _ = writeln!(stdin, "/remember {marker}");
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        while let Ok(line) = rx.try_recv() {
            seen.push_str(&line);
            seen.push('\n');
        }
        if seen.contains("remembered mem_") {
            break;
        }
        assert!(Instant::now() < deadline, "/remember never landed:\n{seen}");
        std::thread::sleep(Duration::from_millis(50));
    }
    let _ = writeln!(stdin, "/quit");
    let _ = child.wait();
}

#[test]
fn backup_then_restore_drill() {
    let a = temp_root("drill-a");
    drive_instance(&a, "drill marker zulu");
    let grant = ka(&a.join("data"))
        .args(["grant", "add", "github:issues:read"])
        .output()
        .unwrap();
    assert!(grant.status.success());
    let link = ka(&a.join("data"))
        .args(["identity", "link", "matrix", "@javier:x.org"])
        .output()
        .unwrap();
    assert!(link.status.success());

    // Backup …
    let cfg = write_config(&a);
    let backup_dir = a.join("bk");
    let out = ka(&a.join("data"))
        .arg("backup")
        .arg(&backup_dir)
        .arg("--config")
        .arg(&cfg)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(backup_dir.join("memory").exists(), "memory in the backup");
    assert!(
        backup_dir.join("config.toml").exists(),
        "config in the backup"
    );

    // … restore into a fresh root …
    let b = temp_root("drill-b");
    let out = ka(&b.join("data"))
        .arg("restore")
        .arg(&backup_dir)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "restore: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    // … and drill the restored instance: state present, memory present,
    // history searchable, and a fresh engine turn still works.
    use rusqlite::OptionalExtension;
    let conn = rusqlite::Connection::open_with_flags(
        b.join("data/state.db"),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .unwrap();
    let hits: Option<i64> = conn
        .query_row(
            "SELECT COUNT(*) FROM episodes_fts WHERE episodes_fts MATCH 'forty'",
            [],
            |r| r.get(0),
        )
        .optional()
        .unwrap();
    assert!(hits.unwrap_or(0) >= 1, "history survived the restore");
    let grants_json: Option<String> = conn
        .query_row("SELECT value FROM meta WHERE key = 'grants'", [], |r| {
            r.get(0)
        })
        .optional()
        .unwrap();
    assert!(
        grants_json
            .as_deref()
            .is_some_and(|j| j.contains("github:issues:read")),
        "grants survived the restore: {grants_json:?}"
    );
    let ids: i64 = conn
        .query_row("SELECT COUNT(*) FROM identities", [], |r| r.get(0))
        .unwrap();
    assert!(ids >= 1, "identities survived the restore");
    let facts = std::fs::read_to_string(b.join("data/memory/facts.md")).unwrap_or_default();
    assert!(
        facts.contains("drill marker zulu"),
        "memory survived: {facts}"
    );

    drive_instance(&b, "post-restore marker");

    // Restore refuses a non-empty root without --force.
    let out = ka(&b.join("data"))
        .arg("restore")
        .arg(&backup_dir)
        .output()
        .unwrap();
    assert!(
        !out.status.success(),
        "restore over a non-empty root must refuse"
    );
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("--force"), "{err}");

    let _ = std::fs::remove_dir_all(&a);
    let _ = std::fs::remove_dir_all(&b);
}

/// F13 (review): the GitHub connector's token must be resolvable. The
/// worker's env sanitizer strips anything matching TOKEN/KEY/…, so the
/// bridge could never see a token from the environment; the worker now
/// re-injects it from the secret-store file tier into the bridge's env.
/// With a token present the tool's failure mode changes from "no GitHub
/// token" to a network/API error — no live GitHub needed.
#[test]
fn github_tools_see_the_secret_store_token() {
    let root = temp_root("gh-token");
    let data = root.join("data");
    let secrets = data.join("secrets");
    std::fs::create_dir_all(&secrets).unwrap();
    std::fs::write(secrets.join("github.token"), "ghp_testtoken123").unwrap();
    let grant = ka(&data)
        .args(["grant", "add", "github:issues:read"])
        .output()
        .unwrap();
    assert!(grant.status.success());

    // Drive the bridge directly: initialize + tools/call.
    use std::io::Write as _;
    let _cfg_holder = write_config(&root);
    let mut child = ka(&data)
        .arg("__mcp")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let stdin = child.stdin.as_mut().unwrap();
    for msg in [
        r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"t"}}}"#,
        r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
        r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"github_issues_list","arguments":{"repo":"ka-agent/ka-agent"}}}"#,
    ] {
        stdin.write_all(msg.as_bytes()).unwrap();
        stdin.write_all(b"\n").unwrap();
    }
    stdin.flush().unwrap();
    drop(child.stdin.take());
    let out = child.wait_with_output().unwrap();
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    let answer = text
        .lines()
        .find(|l| l.contains("\"id\":2"))
        .expect("bridge answered tools/call");
    assert!(
        !answer.contains("no GitHub token"),
        "the token from the secret store did not reach the bridge: {answer}"
    );

    let _ = std::fs::remove_dir_all(&root);
}

/// F9 (review): `restore` must work after a *clean stop*. The daemon only
/// removed its admin socket at startup, so SIGTERM/restart left it behind
/// and restore refused with "daemon appears to be running" — the disaster
/// recovery path failing right after the disaster.
#[test]
fn restore_works_after_a_clean_stop() {
    let root = temp_root("restore-stop");
    let cfg = write_config(&root);
    let data = root.join("data");

    // Run a real daemon so the socket exists, then stop it gracefully
    // (exactly `ka-agent restart`'s mechanism).
    let mut daemon = Daemon {
        child: ka(&data)
            .args(["run", "--config", cfg.to_str().unwrap()])
            .env_remove("KA_MODEL")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    };
    wait_admin_socket(&data, Duration::from_secs(15));
    let sock = data.join("admin.sock");
    assert!(sock.exists(), "socket should exist while running");
    let out = ka(&data)
        .arg("restart")
        .arg("--config")
        .arg(&cfg)
        .output()
        .unwrap();
    assert!(out.status.success());
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        match daemon.child.try_wait().unwrap() {
            Some(code) => {
                assert!(code.success());
                break;
            }
            None if Instant::now() > deadline => panic!("daemon did not stop"),
            None => std::thread::sleep(Duration::from_millis(50)),
        }
    }
    // The socket is gone after a clean stop…
    assert!(
        !sock.exists(),
        "a cleanly stopped daemon must remove its admin socket"
    );

    // …so a backup+restore over that root no longer refuses.
    let backup_dir = root.join("bk");
    let out = ka(&data)
        .arg("backup")
        .arg(&backup_dir)
        .arg("--config")
        .arg(&cfg)
        .output()
        .unwrap();
    assert!(out.status.success());
    let out = ka(&data)
        .arg("restore")
        .arg(&backup_dir)
        .arg("--force")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "restore after a clean stop: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    // And a stale socket (crash simulation) is probed, not feared.
    std::fs::write(&sock, b"").unwrap();
    let out = ka(&data)
        .arg("restore")
        .arg(&backup_dir)
        .arg("--force")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "a stale socket must not block restore: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let _ = std::fs::remove_dir_all(&root);
}
