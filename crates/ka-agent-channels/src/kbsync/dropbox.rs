//! Dropbox → knowledge base: list a folder recursively (cursor deltas on
//! later runs), download each supported file, index it; a `deleted` entry
//! removes its document.

use std::path::Path;

use ka_agent_store::{KbSourceRow, Store};
use serde_json::{Value, json};

use super::{Endpoints, MAX_DOCS, SyncReport, accepted, index_doc, token_of};

pub async fn sync(
    store: &Store,
    src: &KbSourceRow,
    secrets: &Path,
    endpoints: &Endpoints,
) -> Result<SyncReport, String> {
    let token = token_of(src, secrets)?;
    let client = reqwest::Client::new();
    let mut report = SyncReport {
        kb: src.kb_name.clone(),
        ..SyncReport::default()
    };
    let mut entries: Vec<Value> = Vec::new();
    let mut cursor = src.cursor.clone();
    // The first run lists the whole folder; later runs consume the delta.
    let (mut path, mut body) = match &cursor {
        Some(c) => (
            format!("{}/2/files/list_folder/continue", endpoints.dropbox.trim_end_matches('/')),
            json!({"cursor": c}),
        ),
        None => (
            format!("{}/2/files/list_folder", endpoints.dropbox.trim_end_matches('/')),
            json!({"path": src.target, "recursive": true, "include_deleted": true}),
        ),
    };
    loop {
        let resp = client
            .post(&path)
            .bearer_auth(&token)
            .header("Content-Type", "application/json")
            .json(&body)
            .send()
            .await
            .map_err(|e| format!("dropbox list: {e}"))?;
        if !resp.status().is_success() {
            return Err(format!("dropbox list: HTTP {}", resp.status()));
        }
        let value: Value = resp.json().await.map_err(|e| format!("dropbox list: {e}"))?;
        entries.extend(value["entries"].as_array().cloned().unwrap_or_default());
        cursor = value["cursor"].as_str().map(str::to_string);
        if value["has_more"].as_bool() != Some(true) || entries.len() >= MAX_DOCS {
            break;
        }
        path = format!("{}/2/files/list_folder/continue", endpoints.dropbox.trim_end_matches('/'));
        body = json!({"cursor": cursor.clone().unwrap_or_default()});
    }
    let mut seen: Vec<String> = Vec::new();
    for entry in &entries {
        let tag = entry[".tag"].as_str().unwrap_or("");
        let display = entry["path_display"].as_str().unwrap_or("");
        if display.is_empty() {
            continue;
        }
        let path = display.trim_start_matches('/').to_string();
        if tag == "deleted" {
            if store.kb_remove_doc(&src.kb_name, &path).unwrap_or(false) {
                report.removed += 1;
            }
            continue;
        }
        if tag != "file" || !accepted(&path, &src.include_globs) {
            continue;
        }
        if let Some(size) = entry["size"].as_u64()
            && size as usize > super::MAX_BYTES
        {
            continue;
        }
        let resp = client
            .post(format!(
                "{}/2/files/download",
                endpoints.dropbox_content.trim_end_matches('/')
            ))
            .bearer_auth(&token)
            .header(
                "Dropbox-API-Arg",
                json!({"path": display}).to_string(),
            )
            .send()
            .await
            .map_err(|e| format!("dropbox download: {e}"))?;
        if !resp.status().is_success() {
            return Err(format!("dropbox download {display}: HTTP {}", resp.status()));
        }
        let bytes = resp.bytes().await.map_err(|e| format!("dropbox download: {e}"))?;
        if bytes.len() > super::MAX_BYTES {
            continue;
        }
        let content = String::from_utf8_lossy(&bytes).into_owned();
        let title = path.rsplit('/').next().unwrap_or(&path).to_string();
        match index_doc(
            store,
            &src.kb_name,
            &path,
            &title,
            &content,
            "dropbox",
            ka_agent_core::now_ms(),
        ) {
            Ok(true) => report.indexed += 1,
            Ok(false) => report.unchanged += 1,
            Err(_) => {}
        }
        seen.push(path);
    }
    // The delta path never sees the whole folder, so pruning only happens on
    // a full listing (a first run or a cursor reset); deleting a document
    // through Dropbox still removes it via its `deleted` entry.
    if src.cursor.is_none() {
        report.removed += store
            .kb_prune_absent(&src.kb_name, &seen)
            .map_err(|e| e.to_string())?;
    }
    if let Some(cursor) = &cursor {
        store
            .set_kb_source_cursor(&src.kb_name, &src.kind, &src.target, cursor)
            .map_err(|e| e.to_string())?;
    }
    report.cursor = cursor;
    Ok(report)
}
