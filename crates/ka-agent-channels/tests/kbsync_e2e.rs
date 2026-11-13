//! KB sync connectors against fake vendor endpoints: Notion pages appear and
//! a trashed page is pruned; a Dropbox delta with a deleted entry removes
//! its document; git indexes a repository and drops a removed file.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use ka_agent_channels::kbsync::{self, Endpoints, SyncReport};
use ka_agent_store::{KbSourceRow, Store};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// A fake vendor HTTP server: request path → JSON body.
#[derive(Clone, Default)]
struct FakeHttp {
    routes: Arc<Mutex<HashMap<String, Value>>>,
    /// Download bodies (path → bytes) for Dropbox content calls.
    files: Arc<Mutex<HashMap<String, Vec<u8>>>>,
}

impl FakeHttp {
    async fn spawn() -> (Self, String) {
        let fake = Self::default();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        let state = fake.clone();
        tokio::spawn(async move {
            let listener = tokio::net::TcpListener::from_std(listener).unwrap();
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    continue;
                };
                let state = state.clone();
                tokio::spawn(async move {
                    let mut head = Vec::new();
                    let mut buf = [0u8; 1024];
                    let head_len;
                    loop {
                        let Ok(n) = socket.read(&mut buf).await else {
                            return;
                        };
                        if n == 0 {
                            return;
                        }
                        head.extend_from_slice(&buf[..n]);
                        if let Some(i) = head.windows(4).position(|w| w == b"\r\n\r\n") {
                            head_len = i + 4;
                            break;
                        }
                    }
                    let text = String::from_utf8_lossy(&head[..head_len]).into_owned();
                    let path = text
                        .lines()
                        .next()
                        .and_then(|l| l.split_whitespace().nth(1))
                        .unwrap_or("/")
                        .to_string();
                    let length = text
                        .lines()
                        .find_map(|l| l.strip_prefix("Content-Length: ").or_else(|| l.strip_prefix("content-length: ")))
                        .and_then(|v| v.trim().parse::<usize>().ok())
                        .unwrap_or(0);
                    let mut body = head[head_len..].to_vec();
                    while body.len() < length {
                        let mut chunk = vec![0u8; length - body.len()];
                        let Ok(n) = socket.read(&mut chunk).await else {
                            return;
                        };
                        if n == 0 {
                            break;
                        }
                        body.extend_from_slice(&chunk[..n]);
                    }
                    // Dropbox's download endpoint keys off Dropbox-API-Arg.
                    let api_arg = text
                        .lines()
                        .find_map(|l| l.strip_prefix("dropbox-api-arg: ").or_else(|| l.strip_prefix("Dropbox-API-Arg: ")))
                        .unwrap_or("");
                    // The vendor paths carry query strings; routes are
                    // registered without them.
                    let lookup_path = path
                        .split('?')
                        .next()
                        .unwrap_or(&path)
                        .to_string();
                    let key = if api_arg.is_empty() {
                        lookup_path.clone()
                    } else {
                        let parsed: Value = serde_json::from_str(api_arg).unwrap_or(Value::Null);
                        parsed["path"].as_str().unwrap_or(&path).to_string()
                    };
                    let state = state.clone();
                    let payload = {
                        let files = state.files.lock().unwrap();
                        if let Some(bytes) = files.get(&key) {
                            Ok(bytes.clone())
                        } else {
                            let routes = state.routes.lock().unwrap();
                            routes
                                .get(&key)
                                .or_else(|| routes.get(&lookup_path))
                                .map(|v| v.to_string().into_bytes())
                                .ok_or(())
                        }
                    };
                    let (status, body) = match payload {
                        Ok(bytes) => ("200 OK", bytes),
                        Err(()) => ("404 Not Found", b"{}".to_vec()),
                    };
                    let reply = format!(
                        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    );
                    let _ = socket.write_all(reply.as_bytes()).await;
                    let _ = socket.write_all(&body).await;
                });
            }
        });
        (fake, format!("http://{addr}"))
    }

    fn route(&self, path: &str, body: Value) {
        self.routes
            .lock()
            .unwrap()
            .insert(path.to_string(), body);
    }

    fn file(&self, path: &str, bytes: &[u8]) {
        self.files
            .lock()
            .unwrap()
            .insert(path.to_string(), bytes.to_vec());
    }
}

fn kb_with_source(store: &Store, kind: &str, target: &str) -> KbSourceRow {
    store
        .create_kb("docs", "Docs", ka_agent_core::OPERATOR_USER)
        .unwrap();
    let src = KbSourceRow {
        kb_name: "docs".into(),
        kind: kind.into(),
        target: target.into(),
        token_secret: Some(format!("{kind}.token")),
        include_globs: Vec::new(),
        cursor: None,
        schedule_ms: 900_000,
        created_ms: 1,
    };
    store.put_kb_source(&src).unwrap();
    src
}

#[tokio::test]
async fn notion_pages_index_and_trashed_pages_prune() {
    let dir = std::env::temp_dir().join(format!("ka-kbsync-notion-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("secrets")).unwrap();
    std::fs::write(dir.join("secrets/notion.token"), "secret-token").unwrap();
    let store = Store::open(&dir.join("state.db")).unwrap();
    let src = kb_with_source(&store, "notion", "root");
    let (fake, base) = FakeHttp::spawn().await;
    fake.route(
        "/v1/search",
        json!({"results": [
            {"id": "page-1", "properties": {"Name": {"title": [{"plain_text": "Runbook"}]}}}
        ], "has_more": false}),
    );
    fake.route(
        "/v1/blocks/page-1/children",
        json!({"results": [
            {"id": "b1", "type": "heading_1", "has_children": false,
             "heading_1": {"rich_text": [{"plain_text": "Deploy"}]}},
            {"id": "b2", "type": "paragraph", "has_children": false,
             "paragraph": {"rich_text": [{"plain_text": "Rotate the key on friday."}]}}
        ], "has_more": false}),
    );
    let endpoints = Endpoints {
        notion: base.clone(),
        ..Endpoints::default()
    };
    let report = kbsync::sync_with(&store, &src, &dir.join("secrets"), &endpoints)
        .await
        .unwrap();
    assert_eq!(
        report,
        SyncReport {
            kb: "docs".into(),
            indexed: 1,
            unchanged: 0,
            removed: 0,
            cursor: None
        }
    );
    let doc = store.kb_list_docs("docs").unwrap();
    assert_eq!(doc.len(), 1);
    assert_eq!(doc[0].source, "notion");
    let (_, content) = store.kb_get_doc("docs", &doc[0].path).unwrap().unwrap();
    assert!(content.contains("# Deploy"), "{content}");
    assert!(content.contains("Rotate the key"), "{content}");
    // Second run: unchanged, no duplicate.
    let report = kbsync::sync_with(&store, &src, &dir.join("secrets"), &endpoints)
        .await
        .unwrap();
    assert_eq!(report.unchanged, 1);
    assert_eq!(store.kb_doc_count("docs").unwrap(), 1);
    // The page is trashed: search no longer lists it, the doc is pruned.
    fake.route("/v1/search", json!({"results": [], "has_more": false}));
    let report = kbsync::sync_with(&store, &src, &dir.join("secrets"), &endpoints)
        .await
        .unwrap();
    assert_eq!(report.removed, 1);
    assert_eq!(store.kb_doc_count("docs").unwrap(), 0);
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn dropbox_delta_indexes_and_removes() {
    let dir = std::env::temp_dir().join(format!("ka-kbsync-dropbox-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("secrets")).unwrap();
    std::fs::write(dir.join("secrets/dropbox.token"), "secret-token").unwrap();
    let store = Store::open(&dir.join("state.db")).unwrap();
    let src = kb_with_source(&store, "dropbox", "/team");
    let (fake, base) = FakeHttp::spawn().await;
    fake.route(
        "/2/files/list_folder",
        json!({"entries": [
            {".tag": "file", "path_display": "/team/plan.md", "size": 12}
        ], "cursor": "cur-1", "has_more": false}),
    );
    fake.file("/team/plan.md", b"the plan body");
    let endpoints = Endpoints {
        dropbox: base.clone(),
        dropbox_content: base.clone(),
        ..Endpoints::default()
    };
    let report = kbsync::sync_with(&store, &src, &dir.join("secrets"), &endpoints)
        .await
        .unwrap();
    assert_eq!(report.indexed, 1);
    assert_eq!(report.cursor.as_deref(), Some("cur-1"));
    assert_eq!(
        store.kb_sources_for("docs").unwrap()[0].cursor.as_deref(),
        Some("cur-1")
    );
    let doc = store.kb_list_docs("docs").unwrap();
    assert_eq!(doc[0].path, "team/plan.md");
    assert_eq!(doc[0].source, "dropbox");
    // The next sync consumes the delta: a deleted entry removes the doc.
    fake.route(
        "/2/files/list_folder/continue",
        json!({"entries": [
            {".tag": "deleted", "path_display": "/team/plan.md"}
        ], "cursor": "cur-2", "has_more": false}),
    );
    let src = store.kb_sources_for("docs").unwrap()[0].clone();
    let report = kbsync::sync_with(&store, &src, &dir.join("secrets"), &endpoints)
        .await
        .unwrap();
    assert_eq!(report.removed, 1);
    assert_eq!(store.kb_doc_count("docs").unwrap(), 0);
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn git_indexes_a_repository_and_drops_removed_files() {
    let dir = std::env::temp_dir().join(format!("ka-kbsync-git-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let repo = dir.join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    std::fs::write(repo.join("README.md"), "# Repo\nbody").unwrap();
    std::fs::write(repo.join("notes.txt"), "note").unwrap();
    std::fs::write(repo.join("image.png"), "not text").unwrap();
    let git = |args: &[&str]| {
        std::process::Command::new("git")
            .args(args)
            .current_dir(&repo)
            .output()
            .unwrap()
    };
    git(&["init", "-q"]);
    git(&["add", "."]);
    git(&["-c", "user.email=t@example.com", "-c", "user.name=t", "commit", "-qm", "init"]);
    let store = Store::open(&dir.join("state.db")).unwrap();
    store
        .create_kb("docs", "Docs", ka_agent_core::OPERATOR_USER)
        .unwrap();
    let src = KbSourceRow {
        kb_name: "docs".into(),
        kind: "git".into(),
        target: repo.to_string_lossy().into_owned(),
        token_secret: None,
        include_globs: vec!["**/*.md".into(), "**/*.txt".into()],
        cursor: None,
        schedule_ms: 900_000,
        created_ms: 1,
    };
    store.put_kb_source(&src).unwrap();
    let report = kbsync::sync_with(&store, &src, &dir, &Endpoints::default())
        .await
        .unwrap();
    assert_eq!(report.indexed, 2, "{report:?}");
    let docs = store.kb_list_docs("docs").unwrap();
    assert!(docs.iter().any(|d| d.path == "README.md"));
    assert!(docs.iter().any(|d| d.path == "notes.txt"));
    assert!(!docs.iter().any(|d| d.path == "image.png"));
    // Removing a file from the repository removes its document.
    std::fs::remove_file(repo.join("notes.txt")).unwrap();
    git(&["add", "-A"]);
    git(&["-c", "user.email=t@example.com", "-c", "user.name=t", "commit", "-qm", "drop"]);
    let report = kbsync::sync_with(&store, &src, &dir, &Endpoints::default())
        .await
        .unwrap();
    assert_eq!(report.removed, 1, "{report:?}");
    let docs = store.kb_list_docs("docs").unwrap();
    assert!(!docs.iter().any(|d| d.path == "notes.txt"));
    let _ = std::fs::remove_dir_all(&dir);
}
