//! Phase 6 isolation e2e (SEED.md §10 exit criterion): with one instance
//! per operator, nothing crosses data roots — memory, history, grants and
//! identities are per-instance singletons (ADR-0001). Instance A works;
//! instance B must not see any of it.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};
use std::time::Duration;

const MARKER: &str = "isolation marker alpha";

fn temp_root(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("ka-agent-iso-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn write_config(root: &std::path::Path) -> std::path::PathBuf {
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

/// Drive instance A's chat: one scripted turn plus a `/remember`, keeping
/// the process alive until both are observed.
fn drive_instance_a(root: &std::path::Path) {
    let sc = root.join("a.json");
    std::fs::write(
        &sc,
        r#"{ "steps": [ {"text": "the answer is forty-two"} ], "stop": "done", "cost_usd": 0.0, "tokens": 3 }"#,
    )
    .unwrap();
    let cfg = write_config(root);
    let mut child = Command::new(env!("CARGO_BIN_EXE_ka-agent"))
        .args([
            "chat",
            "--config",
            cfg.to_str().unwrap(),
            "--scenario",
            sc.to_str().unwrap(),
        ])
        .env("KA_AGENT_DATA_DIR", root.join("data"))
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
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    let stdin = child.stdin.as_mut().unwrap();
    let _ = writeln!(stdin, "meaning of life?");
    // Positive control first: A answers.
    loop {
        while let Ok(line) = rx.try_recv() {
            seen.push_str(&line);
            seen.push('\n');
        }
        if seen.contains("forty-two") {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "instance A never answered; output:\n{seen}"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    let _ = writeln!(stdin, "/remember {MARKER}");
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    loop {
        while let Ok(line) = rx.try_recv() {
            seen.push_str(&line);
            seen.push('\n');
        }
        if seen.contains("remembered mem_") {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "/remember never landed; output:\n{seen}"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    let _ = writeln!(stdin, "/quit");
    let _ = child.wait();
    // Positive control: the memory file and a grant exist in A.
    let facts = std::fs::read_to_string(root.join("data/memory/facts.md")).unwrap_or_default();
    assert!(facts.contains(MARKER), "A's facts.md missing the marker");
    let grant = Command::new(env!("CARGO_BIN_EXE_ka-agent"))
        .args(["grant", "add", "github:issues:read"])
        .env("KA_AGENT_DATA_DIR", root.join("data"))
        .output()
        .unwrap();
    assert!(grant.status.success(), "grant add in A failed");
}

#[test]
fn nothing_crosses_instances() {
    let a = temp_root("a");
    let b = temp_root("b");
    drive_instance_a(&a);
    write_config(&b);

    // B's durable state: no grant, no identity, and no episode or memory
    // row mentioning A's traffic. An untouched instance has no database at
    // all; if one exists (a daemonless CLI can create it), every table
    // must still be empty.
    use rusqlite::OptionalExtension;
    if !b.join("data/state.db").exists() {
        let facts = std::fs::read_to_string(b.join("data/memory/facts.md")).unwrap_or_default();
        assert!(
            !facts.contains(MARKER),
            "B has no database but its memory carries A's fact"
        );
        let _ = std::fs::remove_dir_all(&a);
        let _ = std::fs::remove_dir_all(&b);
        return;
    }
    let conn = rusqlite::Connection::open_with_flags(
        b.join("data/state.db"),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .unwrap();
    let episodes: Option<i64> = conn
        .query_row(
            "SELECT COUNT(*) FROM episodes_fts WHERE episodes_fts MATCH ?",
            ["isolation OR marker OR forty"],
            |r| r.get(0),
        )
        .optional()
        .unwrap();
    assert_eq!(episodes, Some(0), "B's history must be empty");
    let grants: i64 = conn
        .query_row("SELECT COUNT(*) FROM grants", [], |r| r.get(0))
        .unwrap_or(-1);
    assert_eq!(grants, 0, "B must have no grants");
    let identities: i64 = conn
        .query_row("SELECT COUNT(*) FROM identities", [], |r| r.get(0))
        .unwrap_or(-1);
    assert_eq!(identities, 0, "B must have no linked identities");
    let facts = std::fs::read_to_string(b.join("data/memory/facts.md")).unwrap_or_default();
    assert!(
        !facts.contains(MARKER),
        "B's memory must not contain A's fact"
    );

    // And A still holds its own state (the control side of the control).
    let conn = rusqlite::Connection::open_with_flags(
        a.join("data/state.db"),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .unwrap();
    let grants: i64 = conn
        .query_row("SELECT COUNT(*) FROM grants", [], |r| r.get(0))
        .unwrap();
    assert!(grants >= 1, "A's grant must exist in A");

    let _ = std::fs::remove_dir_all(&a);
    let _ = std::fs::remove_dir_all(&b);
}
