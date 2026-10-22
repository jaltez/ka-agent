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
    assert!(text.contains("ka 0.3.0"), "pin line: {text}");
    assert!(
        text.contains("contracts ") && text.contains("(13 documents)"),
        "contract hash: {text}"
    );
    // A valid config without a model is the one hard failure.
    assert_eq!(out.status.code(), Some(1), "doctor exit: {text}");
    assert!(text.contains("no model — run `ka-agent setup`"), "{text}");
    assert!(text.contains("identities 1 linked"), "{text}");
    assert!(text.contains("no backup yet"), "{text}");
    let hash1: String = text
        .lines()
        .find(|l| l.contains("contracts "))
        .unwrap()
        .chars()
        .skip_while(|c| *c != '(')
        .skip(1)
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
    assert!(
        !text.contains("no backup yet"),
        "backup must be visible after one runs: {text}"
    );
    // The hash is stable across runs (same embedded contract).
    let hash2: String = text
        .lines()
        .find(|l| l.contains("contracts "))
        .unwrap()
        .chars()
        .skip_while(|c| *c != '(')
        .skip(1)
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

    let out = ka(&data).arg("restart").output().unwrap();
    assert!(
        out.status.success(),
        "restart: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("Ka Agent is stopping"),
        "restart reply: {stdout}"
    );

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

/// M11: the `cron_manage` bridge tool operates directly on the store. A
/// fresh bridge lists empty, schedules a recurring job, and the row lands
/// with a valid CronSpec payload.
#[test]
fn cron_manage_tool_schedules_and_lists() {
    let root = temp_root("cron-bridge");
    let data = root.join("data");
    std::fs::create_dir_all(&data).unwrap();
    let _cfg = write_config(&root);

    use std::io::Write as _;
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
        r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"cron_manage","arguments":{"action":"list"}}}"#,
        r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"cron_manage","arguments":{"action":"schedule","every":"1h","text":"bridge smoke"}}}"#,
        r#"{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"cron_manage","arguments":{"action":"remove","id":""}}}"#,
        r#"{"jsonrpc":"2.0","id":5,"method":"tools/call","params":{"name":"cron_manage","arguments":{"action":"remove","id":"nosuchprefix"}}}"#,
        r#"{"jsonrpc":"2.0","id":6,"method":"tools/call","params":{"name":"cron_manage","arguments":{"action":"schedule","every":"5s","text":"too often"}}}"#,
        r#"{"jsonrpc":"2.0","id":7,"method":"tools/call","params":{"name":"cron_manage","arguments":{"action":"schedule","every":"2h","text":"second"}}}"#,
        // Every task id starts `task_`: ambiguous, refused.
        r#"{"jsonrpc":"2.0","id":8,"method":"tools/call","params":{"name":"cron_manage","arguments":{"action":"remove","id":"task_"}}}"#,
        r#"{"jsonrpc":"2.0","id":9,"method":"tools/call","params":{"name":"cron_manage","arguments":{"action":"schedule","repeat":"every weekday at 9","text":"standup notes"}}}"#,
        r#"{"jsonrpc":"2.0","id":10,"method":"tools/call","params":{"name":"cron_manage","arguments":{"action":"schedule","repeat":"every fortnight at 9","text":"nope"}}}"#,
    ] {
        stdin.write_all(msg.as_bytes()).unwrap();
        stdin.write_all(b"\n").unwrap();
    }
    stdin.flush().unwrap();
    drop(child.stdin.take());
    let out = child.wait_with_output().unwrap();
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    let listed = text
        .lines()
        .find(|l| l.contains("\"id\":2"))
        .expect("list answered");
    assert!(!listed.contains("isError"), "{listed}");
    assert!(listed.contains("no crons"), "{listed}");
    let scheduled = text
        .lines()
        .find(|l| l.contains("\"id\":3"))
        .expect("schedule answered");
    assert!(scheduled.contains("scheduled"), "{scheduled}");
    // Review F5: an empty id must be rejected, not match the first job.
    let empty_id = text
        .lines()
        .find(|l| l.contains("\"id\":4"))
        .expect("empty-id remove answered");
    assert!(empty_id.contains("isError"), "{empty_id}");
    let no_match = text
        .lines()
        .find(|l| l.contains("\"id\":5"))
        .expect("no-match remove answered");
    assert!(no_match.contains("isError"), "{no_match}");
    let answer = |id: u32| {
        text.lines()
            .find(|l| l.contains(&format!("\"id\":{id}")))
            .unwrap_or_else(|| panic!("{id} answered: {text}"))
            .to_string()
    };
    let too_often = answer(6);
    assert!(
        too_often.contains("isError") && too_often.contains("at least 60s"),
        "{too_often}"
    );
    let ambiguous = answer(8);
    assert!(
        ambiguous.contains("isError") && ambiguous.contains("several"),
        "{ambiguous}"
    );

    // G3: local-time recurrence from the model, same grammar as /cron.
    let repeat = answer(9);
    assert!(repeat.contains("weekdays at 09:00"), "{repeat}");
    assert!(answer(10).contains("isError"), "{}", answer(10));

    // The valid rows are in the store with a valid payload (the daemon's
    // scheduler will pick them up from here); nothing was removed.
    let store = ka_agent_store::Store::open(&data.join("state.db")).unwrap();
    let crons = store.cron_tasks().unwrap();
    assert_eq!(crons.len(), 3, "{crons:?}");
    let standup = crons
        .iter()
        .find(|t| {
            t.payload
                .as_deref()
                .is_some_and(|p| p.contains("standup notes"))
        })
        .unwrap();
    let spec = ka_agent_core::CronSpec::from_payload(standup.payload.as_deref().unwrap()).unwrap();
    assert_eq!(spec.at_local.as_deref(), Some("09:00"));
    assert_eq!(spec.weekdays, vec![0, 1, 2, 3, 4]);
    let first = crons
        .iter()
        .find(|t| {
            t.payload
                .as_deref()
                .is_some_and(|p| p.contains("bridge smoke"))
        })
        .unwrap();
    assert_eq!(first.kind, "cron");
    let spec = ka_agent_core::CronSpec::from_payload(first.payload.as_deref().unwrap_or(""))
        .expect("valid CronSpec payload");
    assert_eq!(spec.recurrence_ms, Some(3_600_000));
    assert_eq!(spec.mode, ka_agent_core::CronMode::Agent);
    assert_eq!(spec.source, "kagent");

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
    let out = ka(&data).arg("restart").output().unwrap();
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

/// The default backup path rotates: two runs with `--keep 1` leave exactly
/// one timestamped directory (Verification 15).
#[test]
fn backup_default_path_prunes_to_keep() {
    let root = temp_root("rotate");
    let cfg = write_config(&root);
    let data = root.join("data");
    for _ in 0..2 {
        let out = ka(&data)
            .args(["backup", "--keep", "1", "--config"])
            .arg(&cfg)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "backup: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        std::thread::sleep(std::time::Duration::from_millis(1100));
    }
    let backups = data.join("backups");
    let mut stamps: Vec<_> = std::fs::read_dir(&backups)
        .unwrap()
        .flatten()
        .filter(|e| e.path().is_dir())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    stamps.sort();
    assert!(
        !stamps.is_empty(),
        "a backup directory exists: {:?}",
        stamps
    );
    let dirs = stamps.len();
    assert_eq!(dirs, 1, "keep=1 must prune to one: {stamps:?}");
    let _ = std::fs::remove_dir_all(&root);
}

/// G5: a turn started by a cron cannot schedule more crons (loop guard);
/// any other turn can.
#[test]
fn cron_started_turn_cannot_schedule_crons() {
    use std::io::Write as _;
    let root = temp_root("cron-loop");
    let data = root.join("data");
    std::fs::create_dir_all(&data).unwrap();
    let _cfg = write_config(&root);
    let store = ka_agent_store::Store::open(&data.join("state.db")).unwrap();
    let schedule = |source: &str| {
        store.meta_set("turn.source", source).unwrap();
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
            r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"cron_manage","arguments":{"action":"schedule","every":"1h","text":"spawned"}}}"#,
        ] {
            stdin.write_all(msg.as_bytes()).unwrap();
            stdin.write_all(b"\n").unwrap();
        }
        drop(child.stdin.take());
        let out = child.wait_with_output().unwrap();
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .find(|l| l.contains("\"id\":2"))
            .unwrap_or("")
            .to_string()
    };
    let refused = schedule("cron");
    assert!(
        refused.contains("isError") && refused.contains("scheduled job"),
        "{refused}"
    );
    assert!(store.cron_tasks().unwrap().is_empty());
    let allowed = schedule("telegram:42");
    assert!(
        allowed.contains("scheduled") && !allowed.contains("isError"),
        "{allowed}"
    );
    assert_eq!(store.cron_tasks().unwrap().len(), 1);
    let _ = std::fs::remove_dir_all(&root);
}

/// E5: the model can recall past conversations — search, then show.
#[test]
fn bridge_history_tools_find_and_show_past_turns() {
    use std::io::Write as _;
    let root = temp_root("history-tools");
    let data = root.join("data");
    std::fs::create_dir_all(&data).unwrap();
    let _cfg = write_config(&root);
    let store = ka_agent_store::Store::open(&data.join("state.db")).unwrap();
    let session = ka_agent_core::Session::new(root.join("ws"));
    store.insert_session(&session).unwrap();
    let turn = ka_agent_core::Turn::new(session.id.clone(), "where is the Eiffel tower".into());
    store.insert_turn(&turn).unwrap();
    store
        .push_chunk(&ka_agent_core::OutboxChunk::new(
            turn.id.clone(),
            0,
            "In Paris, by the Seine.".into(),
        ))
        .unwrap();
    store
        .index_episode(&session.id, &turn.id, "user", "where is the Eiffel tower")
        .unwrap();
    store
        .index_episode(
            &session.id,
            &turn.id,
            "assistant",
            "In Paris, by the Seine.",
        )
        .unwrap();
    let short = ka_agent_core::ids::short_id(turn.id.as_str()).to_string();

    let mut child = ka(&data)
        .arg("__mcp")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let stdin = child.stdin.as_mut().unwrap();
    let show = format!(
        r#"{{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{{"name":"history_show","arguments":{{"turn":"{short}"}}}}}}"#
    );
    for msg in [
        r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"t"}}}"#.to_string(),
        r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"history_search","arguments":{"query":"Eiffel"}}}"#.to_string(),
        show,
        r#"{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"history_search","arguments":{"query":"zzzz-nothing"}}}"#.to_string(),
    ] {
        stdin.write_all(msg.as_bytes()).unwrap();
        stdin.write_all(b"\n").unwrap();
    }
    drop(child.stdin.take());
    let out = String::from_utf8_lossy(&child.wait_with_output().unwrap().stdout).into_owned();
    let line = |id: u32| {
        out.lines()
            .find(|l| l.contains(&format!("\"id\":{id}")))
            .unwrap_or("")
            .to_string()
    };
    assert!(
        line(2).contains(&format!("[{short}]")) && line(2).contains("Eiffel"),
        "{}",
        line(2)
    );
    assert!(
        line(3).contains("In Paris, by the Seine.") && line(3).contains("where is the Eiffel"),
        "{}",
        line(3)
    );
    assert!(line(4).contains("nothing found"), "{}", line(4));
    let _ = std::fs::remove_dir_all(&root);
}
