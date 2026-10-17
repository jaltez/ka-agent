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
fn drive_instance_a(root: &std::path::Path, marker: &str, grant_scope: &str) {
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
    let _ = writeln!(stdin, "/remember {marker}");
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
    // Positive control: the memory file exists in this instance.
    let facts = std::fs::read_to_string(root.join("data/memory/facts.md")).unwrap_or_default();
    assert!(facts.contains(marker), "facts.md missing its own marker");
    let grant = Command::new(env!("CARGO_BIN_EXE_ka-agent"))
        .args(["grant", "add", grant_scope])
        .env("KA_AGENT_DATA_DIR", root.join("data"))
        .output()
        .unwrap();
    assert!(grant.status.success(), "grant add failed");
}

#[test]
fn nothing_crosses_instances() {
    let a = temp_root("a");
    let b = temp_root("b");
    drive_instance_a(&a, MARKER, "github:issues:read");
    // Drive instance B too (its own scripted turn): an *empty* root proves
    // nothing — the invariant is that a *working* second instance sees none
    // of A's state (F10).
    drive_instance_a(&b, "instance b own fact", "github:pulls:read");

    let conn = rusqlite::Connection::open_with_flags(
        b.join("data/state.db"),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .unwrap();
    let episodes: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM episodes_fts WHERE episodes_fts MATCH ?",
            ["isolation OR marker"],
            |r| r.get(0),
        )
        .unwrap_or(0);
    // B has its own traffic (forty-two), but none of A's marker words.
    assert_eq!(episodes, 0, "B's history must not contain A's marker");
    use rusqlite::OptionalExtension;
    let grants: Option<String> = conn
        .query_row("SELECT value FROM meta WHERE key = 'grants'", [], |r| {
            r.get(0)
        })
        .optional()
        .unwrap();
    // A's grant must not leak; B's own (different) grant is the control.
    assert!(
        !grants
            .as_deref()
            .is_some_and(|j| j.contains("github:issues:read")),
        "A's grant leaked into B: {grants:?}"
    );
    assert!(
        grants
            .as_deref()
            .is_some_and(|j| j.contains("github:pulls:read")),
        "B's own grant missing (the control): {grants:?}"
    );
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
    let grants: Option<String> = conn
        .query_row("SELECT value FROM meta WHERE key = 'grants'", [], |r| {
            r.get(0)
        })
        .optional()
        .unwrap();
    assert!(
        grants
            .as_deref()
            .is_some_and(|j| j.contains("github:issues:read")),
        "A's grant must exist in A: {grants:?}"
    );

    let _ = std::fs::remove_dir_all(&a);
    let _ = std::fs::remove_dir_all(&b);
}
