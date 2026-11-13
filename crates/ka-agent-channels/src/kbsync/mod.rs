//! Knowledge-base sync connectors (M3, D6): Notion, Dropbox and git feed a
//! knowledge base on a schedule. The definition (persona/skills/KB) is
//! shared per KB, never per document: there is no per-document ACL sync.
//!
//! Deletion semantics per source: a Notion page moved to the trash leaves
//! the search results, a Dropbox `deleted` entry removes its document, and a
//! git checkout's missing file removes it. Every sync prunes documents the
//! source no longer reports.

pub mod dropbox;
pub mod git;
pub mod notion;

use std::path::Path;

use ka_agent_store::{KbSourceRow, Store};

/// What one sync did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SyncReport {
    pub kb: String,
    pub indexed: usize,
    pub unchanged: usize,
    pub removed: usize,
    pub cursor: Option<String>,
}

/// Supported text-like formats (the same list uploads accept).
pub const FORMATS: &[&str] = &["md", "txt", "csv", "json"];

/// 2000 documents per KB (the plan's cap).
pub const MAX_DOCS: usize = 2000;
/// 10 MiB per file.
pub const MAX_BYTES: usize = 10 * 1024 * 1024;

/// Whether `path` is a file the KB indexes (extension + optional globs).
pub fn accepted(path: &str, globs: &[String]) -> bool {
    let ext = path
        .rsplit_once('.')
        .map(|(_, e)| e.to_ascii_lowercase())
        .unwrap_or_default();
    if !FORMATS.contains(&ext.as_str()) {
        return false;
    }
    if globs.is_empty() {
        return true;
    }
    globs.iter().any(|g| glob_match(g, path))
}

/// Minimal glob matcher (`*` within a segment, `**` across segments).
pub fn glob_match(pattern: &str, path: &str) -> bool {
    fn matches(p: &[char], s: &[char]) -> bool {
        match (p.first(), s.first()) {
            (None, None) => true,
            (None, Some(_)) => false,
            (Some('*'), _) if p.get(1) == Some(&'*') => {
                let p = &p[2..];
                let p = if p.first() == Some(&'/') { &p[1..] } else { p };
                // `**/` matches zero or more whole segments.
                (0..=s.len()).any(|i| matches(p, &s[i..]))
            }
            (Some('*'), _) => {
                let p = &p[1..];
                (0..=s.len())
                    .take_while(|i| *i == 0 || s.get(i - 1) != Some(&'/'))
                    .any(|i| matches(p, &s[i..]))
            }
            (Some('?'), Some(_)) => matches(&p[1..], &s[1..]),
            (Some(c), Some(sc)) if c == sc => matches(&p[1..], &s[1..]),
            _ => false,
        }
    }
    let p: Vec<char> = pattern.chars().collect();
    let s: Vec<char> = path.chars().collect();
    matches(&p, &s)
}

/// The vendor endpoints (overridable so tests can point at a fake).
#[derive(Debug, Clone)]
pub struct Endpoints {
    pub notion: String,
    pub dropbox: String,
    pub dropbox_content: String,
}

impl Default for Endpoints {
    fn default() -> Self {
        Self {
            notion: "https://api.notion.com".into(),
            dropbox: "https://api.dropboxapi.com".into(),
            dropbox_content: "https://content.dropboxapi.com".into(),
        }
    }
}

/// Run one source's sync against the production endpoints.
pub async fn sync(store: &Store, src: &KbSourceRow, secrets: &Path) -> Result<SyncReport, String> {
    sync_with(store, src, secrets, &Endpoints::default()).await
}

/// The same, with explicit vendor endpoints (tests).
pub async fn sync_with(
    store: &Store,
    src: &KbSourceRow,
    secrets: &Path,
    endpoints: &Endpoints,
) -> Result<SyncReport, String> {
    match src.kind.as_str() {
        "notion" => notion::sync(store, src, secrets, endpoints).await,
        "dropbox" => dropbox::sync(store, src, secrets, endpoints).await,
        "git" => git::sync(store, src).await,
        other => Err(format!("unknown knowledge source kind {other:?}")),
    }
}

/// Index one document if it changed (returns `true` when written).
pub fn index_doc(
    store: &Store,
    kb: &str,
    path: &str,
    title: &str,
    content: &str,
    source: &str,
    mtime_ms: i64,
) -> Result<bool, String> {
    let digest = {
        use sha2::{Digest, Sha256};
        let mut h = Sha256::new();
        h.update(content.as_bytes());
        h.finalize().iter().map(|b| format!("{b:02x}")).collect::<String>()
    };
    if let Ok(docs) = store.kb_list_docs(kb)
        && let Some(existing) = docs.iter().find(|d| d.path == path)
        && existing.sha256 == digest
    {
        return Ok(false);
    }
    store
        .kb_upsert_doc(kb, path, title, content, &digest, source, mtime_ms)
        .map_err(|e| e.to_string())
}

/// A token from the secret store (required by notion/dropbox).
pub fn token_of(src: &KbSourceRow, secrets: &Path) -> Result<String, String> {
    let name = src
        .token_secret
        .as_deref()
        .ok_or_else(|| format!("{} source {}: token_secret is missing", src.kind, src.target))?;
    ka_agent_core::secret::resolve(name, secrets)
        .map_err(|e| format!("{} secret {name}: {e}", src.kind))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[test]
    fn globs_and_formats() {
        assert!(accepted("notes/a.md", &[]));
        assert!(!accepted("image.png", &[]));
        assert!(accepted("a/b/readme.md", &["**/*.md".into()]));
        assert!(accepted("readme.md", &["**/*.md".into()]));
        assert!(accepted("a/b/c.txt", &["a/**/*.txt".into()]));
        assert!(!accepted("a/b/c.txt", &["docs/*.txt".into()]));
        assert!(accepted("docs/x.txt", &["docs/*.txt".into()]));
        assert!(accepted("plan-1.json", &["plan-?.json".into()]));
        assert!(!glob_match("docs/*.md", "docs/sub/a.md"));
        // `**/` matches zero segments: a top-level file matches.
        assert!(glob_match("**/*.md", "README.md"), "zero-segment **/");
        assert!(glob_match("**/*.md", "a/README.md"));
    }
}
