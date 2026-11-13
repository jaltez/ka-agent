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

/// One source a reference pulled into the message (provenance for the
/// share-time check). The store owns the type: it stages and records it.
pub use ka_agent_store::StagedSource as RefSource;

/// Expand every `@reference` in `text` for the acting principal. Text without
/// references is returned unchanged. The returned sources are the ones that
/// actually fed the text (`@kb:` always; `@file` only when the file was
/// readable) — the runtime records them as `turn_sources`.
pub async fn expand(
    store: &Store,
    workspace: &Path,
    who: &ka_agent_core::Principal,
    text: &str,
) -> (String, Vec<RefSource>) {
    if !text.contains('@') {
        return (text.to_string(), Vec::new());
    }
    let mut out = String::with_capacity(text.len());
    let mut word = String::new();
    let mut any = false;
    let mut sources: Vec<RefSource> = Vec::new();
    for ch in text.chars() {
        if ch.is_whitespace() {
            if !word.is_empty() {
                let (expanded, source) = expand_word(store, workspace, who, &word).await;
                any = any || expanded != word;
                if let Some(s) = source
                    && !sources.contains(&s)
                {
                    sources.push(s);
                }
                out.push_str(&expanded);
                word.clear();
            }
            out.push(ch);
        } else {
            word.push(ch);
        }
    }
    if !word.is_empty() {
        let (expanded, source) = expand_word(store, workspace, who, &word).await;
        any = any || expanded != word;
        if let Some(s) = source
            && !sources.contains(&s)
        {
            sources.push(s);
        }
        out.push_str(&expanded);
    }
    if !any {
        return (text.to_string(), Vec::new());
    }
    let out = if out.chars().count() > REF_BUDGET {
        let mut cut: String = out.chars().take(REF_BUDGET).collect();
        cut.push_str("… [refs truncated]");
        cut
    } else {
        out
    };
    (out, sources)
}

/// Expand one reference word for the operator (CLI/terminal/API callers that
/// have no per-user principal).
pub async fn expand_operator(store: &Store, workspace: &Path, text: &str) -> String {
    expand(store, workspace, &ka_agent_core::Principal::operator(), text)
        .await
        .0
}

/// Expand one whitespace-delimited word, if it is a reference token. The
/// second value is the provenance record when the reference actually fed the
/// text.
async fn expand_word(
    store: &Store,
    workspace: &Path,
    who: &ka_agent_core::Principal,
    word: &str,
) -> (String, Option<RefSource>) {
    if let Some(prefix) = word
        .strip_prefix("@turn:")
        .or_else(|| word.strip_prefix("@t:"))
    {
        return (turn_ref(store, prefix, word), None);
    }
    if word.starts_with("@http://") || word.starts_with("@https://") {
        return (url_ref(store, word).await, None);
    }
    if let Some(rest) = word.strip_prefix("@kb:") {
        return kb_ref(store, who, rest, word);
    }
    if let Some(path) = word.strip_prefix('@') {
        let expanded = path_ref(workspace, path, word);
        let source = (!expanded.starts_with("[ref ")).then(|| RefSource {
            kind: "file".into(),
            reference: path.to_string(),
            groups: Vec::new(),
        });
        return (expanded, source);
    }
    (word.to_string(), None)
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
    if !grants.check("http:get:private") && crate::tools::http::http_target_is_private(&host) {
        return format!("[ref {token} refused: private target needs http:get:private]");
    }
    // Blocking fetch off the async executor.
    let fetched = tokio::task::spawn_blocking(move || {
        crate::tools::http::http_get_no_redirect(&parsed, 10, &grants)
    })
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

/// `@kb:<name>` (whole-basis digest) or `@kb:<name>/<path>` (one document):
/// full content, behind the caller's KB access check. An unknown or
/// unauthorized KB answers `no knowledge base matches` — no existence leak.
fn kb_ref(
    store: &Store,
    who: &ka_agent_core::Principal,
    rest: &str,
    token: &str,
) -> (String, Option<RefSource>) {
    let (name, doc_path) = match rest.split_once('/') {
        Some((n, p)) => (n, Some(p)),
        None => (rest, None),
    };
    let Ok(effective) = store.kb_effective(&who.username, &who.role) else {
        return (format!("[ref {token}: no knowledge base matches]"), None);
    };
    if !effective.iter().any(|k| k == name) {
        return (format!("[ref {token}: no knowledge base matches]"), None);
    }
    let groups = store.kb_access(name).unwrap_or_default();
    let source = |path: &str| RefSource {
        kind: "kb".into(),
        reference: format!("kb:{name}/{path}"),
        groups: groups.clone(),
    };
    match doc_path {
        Some(path) => {
            match store.kb_get_doc(name, path) {
                Ok(Some((title, content))) => {
                    // One document is capped at 64 KiB (per the KB contract).
                    let body = ka_agent_core::truncate_at_char(&content, 65_536);
                    (
                        format!("--- @kb:{name}/{path} ({title}) ---\n{body}"),
                        Some(source(path)),
                    )
                }
                _ => (format!("[ref {token}: no such document]"), None),
            }
        }
        None => {
            // The whole KB: every document, in path order, within the
            // message budget.
            let mut docs = String::new();
            let mut used = 0usize;
            let mut sources = Vec::new();
            if let Ok(list) = store.kb_list_docs(name) {
                for doc in list {
                    if used >= REF_BUDGET {
                        docs.push_str("\n… (more documents omitted)");
                        break;
                    }
                    let Ok(Some((title, content))) = store.kb_get_doc(name, &doc.path) else {
                        continue;
                    };
                    let body = ka_agent_core::truncate_at_char(&content, REF_ITEM_MAX);
                    used += body.chars().count();
                    docs.push_str(&format!("--- {title} ({}) ---\n{body}\n", doc.path));
                    sources.push(source(&doc.path));
                }
            }
            if docs.is_empty() {
                return (format!("[ref {token}: knowledge base is empty]"), None);
            }
            // One token can cite several documents; the first is returned as
            // the source and the rest are recorded by the runtime's staging
            // path (this single-value shape keeps the expander simple).
            (format!("--- @kb:{name} ---\n{docs}"), sources.into_iter().next())
        }
    }
}

/// Stage the sources of a message for the turn that is about to be submitted
/// (`record_for_turn` moves them into `turn_sources`).
pub fn stage_sources(store: &Store, source: &str, sources: &[RefSource]) {
    store.stage_turn_sources(source, sources);
}

/// Move one source's staged references into `turn_sources`.
pub fn record_for_turn(store: &Store, source: &str, turn_id: &str) -> usize {
    store.record_staged_sources(source, turn_id)
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
        let out = expand(&store, &ws, &ka_agent_core::Principal::operator(), "resume @nota.md please").await.0;
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
        let out = expand(&store, &ws, &ka_agent_core::Principal::operator(), "look @proj").await.0;
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
        let out = expand(&store, &ws, &ka_agent_core::Principal::operator(), "check @noexiste.md now").await.0;
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
        let out = expand(&store, &ws, &ka_agent_core::Principal::operator(), &format!("@turn:{prefix}")).await.0;
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
        assert_eq!(expand(&store, &ws, &ka_agent_core::Principal::operator(), text).await.0, text);
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
        let out = expand(&store, &ws, &ka_agent_core::Principal::operator(), "@huge.txt").await.0;
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
        let out = expand(&store, &ws, &ka_agent_core::Principal::operator(), "@f0.txt @f1.txt @f2.txt").await.0;
        assert!(
            out.chars().count() <= REF_BUDGET + 40,
            "{}",
            out.chars().count()
        );
        assert!(out.contains("[refs truncated]"), "budget marker missing");
        let _ = std::fs::remove_dir_all(&root);
    }
}
