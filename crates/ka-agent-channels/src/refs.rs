//! `@references` (M11): channel-agnostic inline context injection. A word
//! starting with `@` in any free-text message is replaced by its content
//! before the text reaches `handle_message`, so the expansion is persisted
//! in `turns.inbound_text` (auditable, searchable via FTS).
//!
//! Forms: `@<path>` (workspace-relative or absolute, file or directory),
//! `@turn:<id-prefix>` / `@t:<id-prefix>`, `@http(s)://…` (grant- and
//! SSRF-guarded exactly like the `http_get` tool).
//!
//! Failed references render as visible `[ref …]` markers — never a silent
//! drop. Total expansion budget: 24 000 chars.

use std::path::Path;

use ka_agent_store::Store;

/// Per-reference content cap (chars).
const REF_ITEM_MAX: usize = 8_000;
/// Total expansion budget across one message (chars).
const REF_BUDGET: usize = 24_000;
/// Directory listings: max entries shown.
const DIR_MAX_ENTRIES: usize = 50;
/// Directory listings: max depth.
const DIR_MAX_DEPTH: usize = 2;

/// Expand every `@reference` in `text`. Text without references is returned
/// unchanged.
pub async fn expand(store: &Store, workspace: &Path, text: &str) -> String {
    if !text.contains('@') {
        return text.to_string();
    }
    let mut out = String::with_capacity(text.len());
    let mut word = String::new();
    let mut any = false;
    for ch in text.chars() {
        if ch.is_whitespace() {
            if !word.is_empty() {
                let expanded = expand_word(store, workspace, &word).await;
                any = any || expanded != word;
                out.push_str(&expanded);
                word.clear();
            }
            out.push(ch);
        } else {
            word.push(ch);
        }
    }
    if !word.is_empty() {
        let expanded = expand_word(store, workspace, &word).await;
        any = any || expanded != word;
        out.push_str(&expanded);
    }
    if !any {
        return text.to_string();
    }
    if out.chars().count() > REF_BUDGET {
        let mut cut: String = out.chars().take(REF_BUDGET).collect();
        cut.push_str("… [refs truncated]");
        cut
    } else {
        out
    }
}

/// Expand one whitespace-delimited word, if it is a reference token.
async fn expand_word(store: &Store, workspace: &Path, word: &str) -> String {
    if let Some(prefix) = word
        .strip_prefix("@turn:")
        .or_else(|| word.strip_prefix("@t:"))
    {
        return turn_ref(store, prefix, word);
    }
    if word.starts_with("@http://") || word.starts_with("@https://") {
        return url_ref(store, word).await;
    }
    if let Some(path) = word.strip_prefix('@') {
        return path_ref(workspace, path, word);
    }
    word.to_string()
}

/// `@<path>`: file content or a shallow directory listing.
fn path_ref(workspace: &Path, rel: &str, token: &str) -> String {
    let base = ka_agent_core::expand_home(rel);
    let path = if base.is_absolute() {
        base
    } else {
        workspace.join(base)
    };
    if path.is_file() {
        match read_bounded(&path) {
            Ok(content) => {
                let truncated = content.chars().count() > REF_ITEM_MAX;
                let body = ka_agent_core::truncate_at_char(&content, REF_ITEM_MAX);
                let ellipsis = if truncated {
                    "\n… [ref truncated]"
                } else {
                    ""
                };
                format!("--- {token} ---\n{body}{ellipsis}")
            }
            Err(_) => format!("[ref {token}: unreadable]"),
        }
    } else if path.is_dir() {
        let mut lines = Vec::new();
        let mut count = 0usize;
        list_dir(&path, "", 1, &mut count, &mut lines);
        let mut body = lines.join("\n");
        if count >= DIR_MAX_ENTRIES {
            body.push_str("\n… (more entries omitted)");
        }
        format!("--- {token} ---\n{body}")
    } else {
        format!("[ref {token}: not found]")
    }
}

/// Read at most `REF_ITEM_MAX` chars' worth of bytes from the source (4
/// bytes is the widest UTF-8 char, +4 guarantees the truncation flag is
/// detectable for any content). Never loads an unbounded file — `/dev/zero`
/// or a multi-GB log costs the same as a small note.
fn read_bounded(path: &Path) -> std::io::Result<String> {
    use std::io::Read;
    let mut f = std::fs::File::open(path)?;
    let mut bytes = Vec::new();
    f.by_ref()
        .take((REF_ITEM_MAX as u64) * 4 + 4)
        .read_to_end(&mut bytes)?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

/// Shallow listing: depth-bounded, entry-capped, directories get `/`.
fn list_dir(dir: &Path, prefix: &str, depth: usize, count: &mut usize, out: &mut Vec<String>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut names: Vec<_> = entries.flatten().collect();
    names.sort_by_key(|e| e.file_name());
    for entry in names {
        if *count >= DIR_MAX_ENTRIES {
            return;
        }
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        let is_dir = entry.path().is_dir();
        out.push(format!("{prefix}{name}{}", if is_dir { "/" } else { "" }));
        *count += 1;
        if is_dir && depth < DIR_MAX_DEPTH {
            list_dir(
                &entry.path(),
                &format!("{prefix}{name}/"),
                depth + 1,
                count,
                out,
            );
        }
    }
}

/// `@turn:<prefix>`: the turn's inbound text and its recorded answer.
fn turn_ref(store: &Store, prefix: &str, token: &str) -> String {
    match store.find_turn_by_prefix(prefix) {
        Ok(Some(turn)) => {
            let answer = store
                .chunks_for_turn(&turn.id)
                .map(|chunks| chunks.iter().map(|c| c.text.as_str()).collect::<String>())
                .unwrap_or_default();
            let answer = ka_agent_core::truncate_at_char(&answer, REF_ITEM_MAX);
            format!("--- {token} ---\n> {}\n{answer}", turn.inbound_text)
        }
        _ => format!("[ref {token}: not found]"),
    }
}

/// `@http(s)://…`: the `http_get` tool's grant and SSRF rules, applied to a
/// reference. Refusal is visible, never a dropped message.
async fn url_ref(store: &Store, token: &str) -> String {
    let full = &token[1..]; // strip the leading '@'
    let parsed = match url::Url::parse(full) {
        Ok(u) => u,
        Err(e) => return format!("[ref {token}: bad url: {e}]"),
    };
    if parsed.scheme() != "http" && parsed.scheme() != "https" {
        return format!("[ref {token}: unsupported scheme]");
    }
    let host = parsed.host_str().unwrap_or("").to_string();
    let grants = crate::grants::Grants::load(store).unwrap_or_default();
    let scope = format!("http:get:{host}");
    if !grants.check(&scope) {
        return format!("[ref {token} refused: no grant for {scope}]");
    }
    if !grants.check("http:get:private") && crate::mcp::http_target_is_private(&host) {
        return format!("[ref {token} refused: private target needs http:get:private]");
    }
    // Blocking fetch off the async executor.
    let fetched =
        tokio::task::spawn_blocking(move || crate::mcp::http_get_no_redirect(&parsed, 10, &grants))
            .await
            .unwrap_or_else(|e| Err(format!("join: {e}")));
    match fetched {
        Ok(body) => {
            let cut = ka_agent_core::truncate_at_char(&body, REF_ITEM_MAX);
            let ellipsis = if body.chars().count() > REF_ITEM_MAX {
                "\n… [ref truncated]"
            } else {
                ""
            };
            format!("--- {token} ---\n{cut}{ellipsis}")
        }
        Err(e) => format!("[ref {token}: {e}]"),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use ka_agent_core::{OutboxChunk, OutboxStatus, Session, Turn, TurnStatus};

    fn temp_root(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("ka-refs-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn store_at(dir: &std::path::Path) -> Store {
        Store::open(&dir.join("state.db")).unwrap()
    }

    #[tokio::test]
    async fn file_ref_injects_content() {
        let root = temp_root("file");
        let ws = root.join("ws");
        std::fs::create_dir_all(&ws).unwrap();
        std::fs::write(ws.join("nota.md"), "CONTENIDO-UNO").unwrap();
        let store = store_at(&root.join("data"));
        let out = expand(&store, &ws, "resume @nota.md please").await;
        assert_eq!(out, "resume --- @nota.md ---\nCONTENIDO-UNO please");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn dir_ref_lists_two_levels() {
        let root = temp_root("dir");
        let ws = root.join("ws");
        std::fs::create_dir_all(ws.join("proj/sub/deep")).unwrap();
        std::fs::write(ws.join("proj/a.md"), "a").unwrap();
        std::fs::write(ws.join("proj/sub/b.md"), "b").unwrap();
        std::fs::write(ws.join("proj/sub/deep/c.md"), "c").unwrap();
        let store = store_at(&root.join("data"));
        let out = expand(&store, &ws, "look @proj").await;
        assert!(out.contains("--- @proj ---"), "{out}");
        assert!(out.contains("a.md"), "{out}");
        assert!(out.contains("sub/b.md"), "{out}");
        // depth 2 only: the grandchild file is not listed (its dir is).
        assert!(!out.contains("deep/c.md"), "{out}");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn missing_ref_is_visible_not_dropped() {
        let root = temp_root("missing");
        let ws = root.join("ws");
        std::fs::create_dir_all(&ws).unwrap();
        let store = store_at(&root.join("data"));
        let out = expand(&store, &ws, "check @noexiste.md now").await;
        assert_eq!(out, "check [ref @noexiste.md: not found] now");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn turn_ref_recovers_a_seeded_turn() {
        let root = temp_root("turn");
        let ws = root.join("ws");
        std::fs::create_dir_all(&ws).unwrap();
        let store = store_at(&root.join("data"));
        let session = Session::new(ws.clone());
        store.insert_session(&session).unwrap();
        let mut turn = Turn::new(session.id.clone(), "pregunta sembrada".into());
        store.insert_turn(&turn).unwrap();
        turn.transition(TurnStatus::Submitted).unwrap();
        store.update_turn(&turn).unwrap();
        turn.transition(TurnStatus::Running).unwrap();
        store.update_turn(&turn).unwrap();
        turn.transition(TurnStatus::Done).unwrap();
        store.update_turn(&turn).unwrap();
        store
            .push_chunk(&OutboxChunk {
                turn: turn.id.clone(),
                chunk_index: 0,
                text: "respuesta sembrada".into(),
                status: OutboxStatus::Queued,
                channel_message_id: None,
                created_ms: 0,
            })
            .unwrap();
        let prefix = ka_agent_core::ids::short_id(turn.id.as_str());
        let out = expand(&store, &ws, &format!("@turn:{prefix}")).await;
        assert!(out.contains(&format!("--- @turn:{prefix} ---")), "{out}");
        assert!(out.contains("> pregunta sembrada"), "{out}");
        assert!(out.contains("respuesta sembrada"), "{out}");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn plain_text_is_untouched_and_emails_are_not_refs() {
        let root = temp_root("plain");
        let ws = root.join("ws");
        std::fs::create_dir_all(&ws).unwrap();
        let store = store_at(&root.join("data"));
        let text = "plain text\nsecond line, mail me at bob@example.com ok";
        assert_eq!(expand(&store, &ws, text).await, text);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn huge_file_ref_is_read_bounded() {
        let root = temp_root("huge");
        let ws = root.join("ws");
        std::fs::create_dir_all(&ws).unwrap();
        // Far beyond the 8k cap: the read itself must be bounded and the
        // expansion truncated, never an unbounded load.
        std::fs::write(ws.join("huge.txt"), "y".repeat(400_000)).unwrap();
        let store = store_at(&root.join("data"));
        let out = expand(&store, &ws, "@huge.txt").await;
        assert!(out.contains("[ref truncated]"), "{out}");
        assert!(out.chars().count() < 9_000, "{}", out.chars().count());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn total_budget_truncates() {
        let root = temp_root("budget");
        let ws = root.join("ws");
        std::fs::create_dir_all(&ws).unwrap();
        // Three refs of 9k chars each: the last is cut by the 24k budget.
        for i in 0..3 {
            std::fs::write(ws.join(format!("f{i}.txt")), "x".repeat(9_000)).unwrap();
        }
        let store = store_at(&root.join("data"));
        let out = expand(&store, &ws, "@f0.txt @f1.txt @f2.txt").await;
        assert!(
            out.chars().count() <= REF_BUDGET + 40,
            "{}",
            out.chars().count()
        );
        assert!(out.contains("[refs truncated]"), "budget marker missing");
        let _ = std::fs::remove_dir_all(&root);
    }
}
