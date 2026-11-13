//! Offboarding operations (ADR-0004, D10): export and purge one user's
//! partition. Shared by the CLI (`ka-agent user export|purge`) and the web
//! users page, so both do exactly the same thing.
//!
//! Export writes one tar with the user's session/memory/notes/attachment/
//! workspace trees. Purge removes them. The account row and the audit chain
//! are never touched by either: disabling is reversible, and the chain is
//! immutable.

use crate::paths::Paths;

/// The directories an export/purge covers, relative to the user's root.
const TREES: &[&str] = &[
    "memory",
    "notes",
    "attachments",
    "workspace",
    "ka",
    "engine-home",
    "engine-state",
];

/// Write `<data>/backups/user-<username>-<ts_ms>.tar` with the user's trees.
/// Returns the archive path.
pub fn export_user(paths: &Paths, username: &str) -> Result<std::path::PathBuf, String> {
    let root = paths.user_root(username);
    if username == ka_agent_core::OPERATOR_USER {
        return Err(
            "the operator's own data root is the profile — export it with `ka-agent backup`"
                .to_string(),
        );
    }
    if !root.exists() {
        return Err(format!("no data directory for {username}"));
    }
    std::fs::create_dir_all(paths.backups()).map_err(|e| format!("backups dir: {e}"))?;
    let archive = paths
        .backups()
        .join(format!("user-{username}-{}.tar", ka_agent_core::now_ms()));
    let file = std::fs::File::create(&archive).map_err(|e| format!("create archive: {e}"))?;
    let mut builder = tar::Builder::new(file);
    // Never follow links: a planted symlink must not read files outside the
    // user's partition into the export (links are stored as link entries).
    builder.follow_symlinks(false);
    for tree in TREES {
        let dir = root.join(tree);
        if !dir.exists() {
            continue;
        }
        builder
            .append_dir_all(tree, &dir)
            .map_err(|e| format!("archive {tree}: {e}"))?;
    }
    builder.finish().map_err(|e| format!("finish archive: {e}"))?;
    Ok(archive)
}

/// Delete the user's trees (the account row and the audit chain stay).
/// Returns the trees that existed and were removed.
pub fn purge_user(paths: &Paths, username: &str) -> Result<Vec<String>, String> {
    if username == ka_agent_core::OPERATOR_USER {
        return Err(
            "refusing to purge the operator — that is the profile itself (`ka-agent profile remove`)"
                .to_string(),
        );
    }
    let root = paths.user_root(username);
    if !root.exists() {
        return Ok(Vec::new());
    }
    let mut removed = Vec::new();
    for tree in TREES {
        let dir = root.join(tree);
        if dir.exists() {
            std::fs::remove_dir_all(&dir).map_err(|e| format!("remove {tree}: {e}"))?;
            removed.push((*tree).to_string());
        }
    }
    Ok(removed)
}

/// Whether an export exists for this user (the purge warning names it).
pub fn latest_export(paths: &Paths, username: &str) -> Option<std::path::PathBuf> {
    let prefix = format!("user-{username}-");
    let mut hits: Vec<std::path::PathBuf> = std::fs::read_dir(paths.backups())
        .ok()?
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| {
                    // Exact username match: the timestamp tail must be all
                    // digits, or `ana` would match `ana-2`'s exports.
                    n.strip_prefix(&prefix)
                        .is_some_and(|tail| {
                            tail.strip_suffix(".tar")
                                .is_some_and(|t| !t.is_empty() && t.chars().all(char::is_numeric))
                        })
                })
        })
        .collect();
    hits.sort();
    hits.pop()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[test]
    fn export_then_purge_one_partition() {
        let dir = std::env::temp_dir().join(format!("ka-user-ops-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let paths = Paths::new(&dir);
        paths.ensure_dirs().unwrap();
        paths.ensure_user_dirs("ana").unwrap();
        std::fs::write(paths.user_notes("ana").join("plan.md"), "hello").unwrap();

        let archive = export_user(&paths, "ana").unwrap();
        assert!(archive.exists());
        assert_eq!(latest_export(&paths, "ana").as_deref(), Some(archive.as_path()));
        // The archive carries the note.
        let text = std::fs::read(&archive).unwrap();
        assert!(text.windows(5).any(|w| w == b"plan."), "tar names the note");

        let removed = purge_user(&paths, "ana").unwrap();
        assert!(removed.contains(&"notes".to_string()));
        assert!(!paths.user_notes("ana").join("plan.md").exists());
        // The user's root survives (the account row still exists on disk).
        assert!(paths.user_root("ana").exists());
        // The operator's root is refused.
        assert!(purge_user(&paths, ka_agent_core::OPERATOR_USER).is_err());
        assert!(export_user(&paths, ka_agent_core::OPERATOR_USER).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
