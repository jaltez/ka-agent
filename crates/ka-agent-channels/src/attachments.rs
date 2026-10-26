//! The inbound attachment inbox (SEED.md §5.4): download a message's
//! photo or document into `<workspace>/attachments/` and return the
//! workspace-relative path. `@reference` expansion resolves against the
//! workspace, so the operator's next message can use the file by pasting
//! the returned `@attachments/…` token.
//!
//! Split from telegram.rs (moves only).

use std::io::Write as _;
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
    save_attachment_bytes(workspace, &name, &bytes)
}

/// Write `bytes` to `<workspace>/attachments/<name>` and return the
/// workspace-relative path. Collisions never overwrite: the first free
/// `-N` suffix wins.
///
/// The name is sanitized here before anything is created: it alone decides
/// where the bytes land, so the destination is validated before the write
/// instead of after it. Files are created exclusively (`create_new`), so
/// two writers racing for one name cannot clobber each other.
pub(crate) fn save_attachment_bytes(
    workspace: &Path,
    name: &str,
    bytes: &[u8],
) -> Result<String, String> {
    let Some(name) = sanitize_attachment_name(name) else {
        return Err(format!("unusable attachment name {name:?}"));
    };
    let dir = workspace.join("attachments");
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let mut n = 1;
    loop {
        let candidate = if n == 1 {
            name.clone()
        } else {
            suffixed(&name, n)
        };
        // A single sanitized component: `dir.join` cannot leave `dir`.
        let rel = Path::new("attachments").join(&candidate);
        let path = workspace.join(&rel);
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(mut file) => {
                if let Err(e) = file.write_all(bytes) {
                    // Do not leave a half-written file behind to collide
                    // with the next save.
                    let _ = std::fs::remove_file(&path);
                    return Err(e.to_string());
                }
                return Ok(rel.display().to_string());
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => n += 1,
            Err(e) => return Err(e.to_string()),
        }
    }
}

/// `note.txt` at collision `2` → `note-2.txt`; an extensionless
/// `note` → `note-2`.
fn suffixed(name: &str, n: usize) -> String {
    match name.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() && !ext.is_empty() => {
            format!("{stem}-{n}.{ext}")
        }
        Some((stem, _)) if !stem.is_empty() => format!("{stem}-{n}"),
        _ => format!("{name}-{n}"),
    }
}

/// `[A-Za-z0-9._-]` only, no leading dot, ≤ 64 chars, never empty.
pub(crate) fn sanitize_attachment_name(raw: &str) -> Option<String> {
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

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU32, Ordering};

    static DIR_SEQ: AtomicU32 = AtomicU32::new(0);

    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "ka-att-{tag}-{}-{}",
            std::process::id(),
            DIR_SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn a_name_that_escapes_is_confined_to_the_attachments_dir() {
        let ws = scratch("escape");
        let rel = save_attachment_bytes(&ws, "../../evil.txt", b"x").unwrap();
        assert!(
            rel.starts_with("attachments/"),
            "returned path must stay in the inbox: {rel}"
        );
        let written = ws.join(&rel);
        assert!(written.is_file());
        let inside = written.canonicalize().unwrap();
        assert!(inside.starts_with(ws.canonicalize().unwrap()), "{inside:?}");
        assert!(!ws.parent().unwrap().join("evil.txt").exists());
        let _ = std::fs::remove_dir_all(&ws);
    }

    #[test]
    fn an_unusable_name_is_an_error() {
        let ws = scratch("unusable");
        assert!(save_attachment_bytes(&ws, "..", b"x").is_err());
        assert!(save_attachment_bytes(&ws, "", b"x").is_err());
        assert!(save_attachment_bytes(&ws, ".", b"x").is_err());
        let inbox = ws.join("attachments");
        assert!(
            !inbox.exists() || !inbox.read_dir().unwrap().any(|e| e.is_ok()),
            "nothing may be written for an unusable name"
        );
        let _ = std::fs::remove_dir_all(&ws);
    }

    #[test]
    fn collisions_never_overwrite() {
        let ws = scratch("collide");
        let first = save_attachment_bytes(&ws, "note.txt", b"first").unwrap();
        let second = save_attachment_bytes(&ws, "note.txt", b"second").unwrap();
        assert_eq!(first, "attachments/note.txt");
        assert_eq!(second, "attachments/note-2.txt");
        assert_eq!(std::fs::read(ws.join(&first)).unwrap(), b"first");
        assert_eq!(std::fs::read(ws.join(&second)).unwrap(), b"second");
        let plain = save_attachment_bytes(&ws, "log", b"one").unwrap();
        let suffixed = save_attachment_bytes(&ws, "log", b"two").unwrap();
        assert_eq!(plain, "attachments/log");
        assert_eq!(suffixed, "attachments/log-2");
        let _ = std::fs::remove_dir_all(&ws);
    }

    /// The name check and the write used to be separate steps (`exists` then
    /// `write`), so two writers could pick the same path and one overwrote
    /// the other. `create_new` makes the winner unique.
    #[test]
    fn concurrent_saves_get_distinct_names() {
        let ws = scratch("race");
        let bytes = vec![b'x'; 64 * 1024];
        let handles: Vec<_> = (0..16)
            .map(|_| {
                let ws = ws.clone();
                let bytes = bytes.clone();
                std::thread::spawn(move || save_attachment_bytes(&ws, "same.bin", &bytes).unwrap())
            })
            .collect();
        let paths: Vec<String> = handles.into_iter().map(|h| h.join().unwrap()).collect();
        let unique: std::collections::HashSet<&String> = paths.iter().collect();
        assert_eq!(unique.len(), paths.len(), "duplicate paths: {paths:?}");
        for path in &paths {
            assert_eq!(std::fs::read(ws.join(path)).unwrap().len(), bytes.len());
        }
        let _ = std::fs::remove_dir_all(&ws);
    }
}
