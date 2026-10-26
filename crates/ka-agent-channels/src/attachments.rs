//! The inbound attachment inbox (SEED.md §5.4): download a message's
//! photo or document into `<workspace>/attachments/` and return the
//! workspace-relative path. `@reference` expansion resolves against the
//! workspace, so the operator's next message can use the file by pasting
//! the returned `@attachments/…` token.
//!
//! Split from telegram.rs (moves only).

use std::path::Path;

use serde_json::Value;

use crate::telegram::TelegramClient;
use crate::telegram::short_id;

pub(crate) async fn save_attachment(
    client: &TelegramClient,
    workspace: &Path,
    message: &Value,
) -> Result<String, String> {
    // Photos arrive as size-ascending variants; the last is the best.
    let (file_id, name): (String, String) = if let Some(sizes) = message["photo"].as_array() {
        let best = sizes
            .iter()
            .filter_map(|p| p["file_id"].as_str())
            .next_back()
            .unwrap_or("");
        if best.is_empty() {
            return Err("photo without a file_id".into());
        }
        (best.to_string(), format!("photo-{}.jpg", short_id(best, 8)))
    } else if let Some(doc) = message["document"].as_object() {
        let file_id = doc
            .get("file_id")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        if file_id.is_empty() {
            return Err("document without a file_id".into());
        }
        let raw = doc.get("file_name").and_then(Value::as_str).unwrap_or("");
        let name = sanitize_attachment_name(raw)
            .unwrap_or_else(|| format!("doc-{}.bin", short_id(&file_id, 8)));
        (file_id, name)
    } else {
        return Err("message carries no photo or document".into());
    };
    let file_path = client.get_file(&file_id).await.map_err(|e| e.to_string())?;
    let bytes = client
        .download_file(&file_path)
        .await
        .map_err(|e| e.to_string())?;
    let dir = workspace.join("attachments");
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    // Collisions never overwrite: first free `-N` suffix wins.
    let mut path = dir.join(&name);
    let mut n = 1;
    while path.exists() {
        n += 1;
        let stem = name.rsplit_once('.').map_or(name.as_str(), |(s, _)| s);
        let ext = name.rsplit_once('.').map_or("", |(_, e)| e);
        let candidate = if ext.is_empty() {
            format!("{stem}-{n}")
        } else {
            format!("{stem}-{n}.{ext}")
        };
        path = dir.join(candidate);
    }
    std::fs::write(&path, &bytes).map_err(|e| e.to_string())?;
    path.strip_prefix(workspace)
        .map(|p| p.display().to_string())
        .map_err(|_| "attachment escaped the workspace".to_string())
}

/// `[A-Za-z0-9._-]` only, no leading dot, ≤ 64 chars, never empty.
fn sanitize_attachment_name(raw: &str) -> Option<String> {
    let mut out = String::new();
    for c in raw.chars() {
        if c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-' {
            out.push(c);
        } else {
            out.push('_');
        }
    }
    while out.starts_with('.') {
        out.remove(0);
    }
    while out.contains("..") {
        out = out.replace("..", ".");
    }
    let trimmed: String = out.chars().take(64).collect();
    (!trimmed.is_empty()).then_some(trimmed)
}
