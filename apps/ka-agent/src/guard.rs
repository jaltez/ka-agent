//! `ka-agent __guard`: the engine's pre_tool_use hook that keeps the
//! assistant out of ka-agent's own files. The engine matches rules on the
//! raw path argument, so `write { path: "../config.toml" }` from the
//! workspace never hits a pattern rule; this hook normalizes the path
//! (`~`, `.`, `..`, symlinks) and refuses when it lands inside a
//! protected list.
//!
//! Contract (ka engine hooks): the tool call arrives as JSON on stdin
//! `{"tool","arguments","cwd"}`; exit 2 blocks the call and stderr is the
//! reason the model sees. Any other exit allows it.

use std::path::{Component, Path, PathBuf};

use serde_json::Value;

/// Protected lists, from the worker env (set by the daemon).
fn protected(var: &str) -> Vec<PathBuf> {
    std::env::var_os(var)
        .map(|joined| std::env::split_paths(&joined).collect())
        .unwrap_or_default()
}

/// Lexical normalization: resolve `.`/`..` without touching the disk.
fn lexical(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for comp in path.components() {
        match comp {
            Component::ParentDir => {
                if !out.pop() {
                    out.push("..");
                }
            }
            Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// The real path: canonicalize the nearest existing ancestor and re-append
/// the rest, so a symlinked directory cannot smuggle a protected path.
fn resolve(path: &Path) -> PathBuf {
    let lexical = lexical(path);
    let mut probe = lexical.clone();
    let mut rest: Vec<std::ffi::OsString> = Vec::new();
    while !probe.exists() {
        let Some(name) = probe.file_name().map(|n| n.to_os_string()) else {
            break;
        };
        rest.push(name);
        if !probe.pop() {
            break;
        }
    }
    let mut base = std::fs::canonicalize(&probe).unwrap_or(probe);
    for name in rest.into_iter().rev() {
        base.push(name);
    }
    base
}

/// Is `path` equal to or beneath any protected entry?
fn inside(path: &Path, list: &[PathBuf]) -> Option<PathBuf> {
    // Both sides go through `resolve`: a candidate built from a symlinked
    // spelling of the same directory (macOS `/var` → `/private/var`) must
    // match the resolved root, or the guard stops protecting its files.
    let path = resolve(path);
    for entry in list {
        let entry = resolve(entry);
        if path == entry || path.starts_with(&entry) {
            return Some(entry);
        }
    }
    None
}

/// The path argument of a file tool (`grep` may omit it: then `cwd`).
fn target_path(tool: &str, args: &Value, cwd: &Path) -> Option<PathBuf> {
    let raw = args
        .get("path")
        .and_then(Value::as_str)
        .or_else(|| args.get("file").and_then(Value::as_str))
        .map(str::to_string);
    let raw = match raw {
        Some(r) => r,
        // grep/glob legitimately search the whole cwd; glob never writes
        None if tool == "grep" || tool == "glob" => cwd.display().to_string(),
        None => return None,
    };
    let expanded = if let Some(rest) = raw.strip_prefix("~/") {
        let home = std::env::var_os("HOME").map(PathBuf::from)?;
        home.join(rest).display().to_string()
    } else {
        raw
    };
    let p = PathBuf::from(expanded);
    Some(if p.is_absolute() { p } else { cwd.join(p) })
}

pub fn run() -> std::process::ExitCode {
    let mut input = String::new();
    if std::io::Read::read_to_string(&mut std::io::stdin(), &mut input).is_err() {
        return std::process::ExitCode::SUCCESS;
    }
    let Ok(payload) = serde_json::from_str::<Value>(input.trim()) else {
        return std::process::ExitCode::SUCCESS;
    };
    let tool = payload["tool"].as_str().unwrap_or("");
    let args = payload.get("arguments").cloned().unwrap_or(Value::Null);
    let cwd = payload["cwd"]
        .as_str()
        .filter(|c| !c.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_else(|| PathBuf::from("/"));
    let Some(target) = target_path(tool, &args, &cwd) else {
        return std::process::ExitCode::SUCCESS;
    };
    let resolved = resolve(&target);
    let (list, verb) = match tool {
        "write" | "edit" => (protected("KA_AGENT_GUARD_WRITE"), "write"),
        "read" | "grep" => (protected("KA_AGENT_GUARD_READ"), "read"),
        _ => return std::process::ExitCode::SUCCESS,
    };
    if let Some(hit) = inside(&resolved, &list) {
        eprintln!(
            "protected: ka-agent's own files ({} is inside {})",
            resolved.display(),
            hit.display()
        );
        return std::process::ExitCode::from(2);
    }
    let _ = verb;
    std::process::ExitCode::SUCCESS
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    #[test]
    fn lexical_resolves_dotdot_without_disk() {
        assert_eq!(lexical(Path::new("/a/b/../c")), PathBuf::from("/a/c"));
        assert_eq!(lexical(Path::new("a/./b")), PathBuf::from("a/b"));
        assert_eq!(lexical(Path::new("../x")), PathBuf::from("../x"));
    }

    #[test]
    fn inside_matches_equal_and_beneath() {
        let dir = std::env::temp_dir().join(format!("ka-guard-{}", std::process::id()));
        let secret = dir.join("secrets");
        std::fs::create_dir_all(&secret).unwrap();
        assert!(inside(&secret.join("x"), std::slice::from_ref(&secret)).is_some());
        assert!(inside(&secret, std::slice::from_ref(&secret)).is_some());
        assert!(inside(&dir.join("other"), &[secret]).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn dotdot_escape_from_the_workspace_is_caught() {
        let root = std::env::temp_dir().join(format!("ka-guard-esc-{}", std::process::id()));
        let ws = root.join("ws");
        std::fs::create_dir_all(&ws).unwrap();
        std::fs::write(root.join("config.toml"), "x").unwrap();
        let target = target_path("write", &serde_json::json!({"path": "../config.toml"}), &ws)
            .expect("path");
        let resolved = resolve(&target);
        assert!(
            inside(&resolved, &[root.join("config.toml")]).is_some(),
            "{}",
            resolved.display()
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn grep_without_a_path_checks_the_cwd() {
        let ws = std::env::temp_dir().join(format!("ka-guard-grep-{}", std::process::id()));
        std::fs::create_dir_all(ws.join("secrets")).unwrap();
        // From the workspace root a bare grep is fine; run from inside the
        // protected dir it is refused.
        let target = target_path("grep", &serde_json::json!({"pattern": "x"}), &ws).expect("cwd");
        assert!(inside(&resolve(&target), &[ws.join("secrets")]).is_none());
        let inside_dir = target_path(
            "grep",
            &serde_json::json!({"pattern": "x"}),
            &ws.join("secrets"),
        )
        .expect("cwd");
        assert!(
            inside(&resolve(&inside_dir), &[ws.join("secrets")]).is_some(),
            "a cwd search inside a protected dir is refused"
        );
        let _ = std::fs::remove_dir_all(&ws);
    }
}
