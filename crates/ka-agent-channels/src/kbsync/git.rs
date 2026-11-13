//! git → knowledge base: a shallow clone (or fetch) of a repository, every
//! supported file indexed; a file removed from the repository is removed
//! from the KB.

use ka_agent_store::{KbSourceRow, Store};

use super::{MAX_DOCS, MAX_BYTES, SyncReport, accepted};

pub async fn sync(store: &Store, src: &KbSourceRow) -> Result<SyncReport, String> {
    // A dedicated checkout per (kb, url) keeps runs incremental.
    let root = std::env::temp_dir().join(format!(
        "ka-kb-git-{}-{}",
        slug(&src.kb_name),
        slug(&src.target)
    ));
    let mut report = SyncReport {
        kb: src.kb_name.clone(),
        ..SyncReport::default()
    };
    let git = which_git().await?;
    if root.join(".git").exists() {
        run(&git, &["-C", &root.to_string_lossy(), "fetch", "--depth", "1", "origin"]).await?;
        run(
            &git,
            &["-C", &root.to_string_lossy(), "reset", "--hard", "origin/HEAD"],
        )
        .await?;
    } else {
        let _ = std::fs::remove_dir_all(&root);
        run(
            &git,
            &[
                "clone",
                "--depth",
                "1",
                &src.target,
                &root.to_string_lossy(),
            ],
        )
        .await?;
    }
    let mut seen = Vec::new();
    let mut stack = vec![root.clone()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n == ".git")
            {
                continue;
            }
            if path.is_dir() {
                stack.push(path);
                continue;
            }
            let Ok(rel) = path.strip_prefix(&root) else {
                continue;
            };
            let rel = rel.to_string_lossy().replace('\\', "/");
            if !accepted(&rel, &src.include_globs) {
                continue;
            }
            let Ok(meta) = entry.metadata() else { continue };
            if meta.len() as usize > MAX_BYTES {
                continue;
            }
            let Ok(content) = std::fs::read_to_string(&path) else {
                continue;
            };
            let title = rel.rsplit('/').next().unwrap_or(&rel).to_string();
            let mtime = meta
                .modified()
                .ok()
                .and_then(|m| m.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_millis() as i64)
                .unwrap_or_else(ka_agent_core::now_ms);
            match super::index_doc(store, &src.kb_name, &rel, &title, &content, "git", mtime) {
                Ok(true) => report.indexed += 1,
                Ok(false) => report.unchanged += 1,
                Err(_) => {}
            }
            seen.push(rel);
            if seen.len() >= MAX_DOCS {
                break;
            }
        }
    }
    report.removed = store
        .kb_prune_absent(&src.kb_name, &seen)
        .map_err(|e| e.to_string())?;
    Ok(report)
}

async fn which_git() -> Result<String, String> {
    let out = tokio::process::Command::new("git")
        .arg("--version")
        .output()
        .await
        .map_err(|_| {
            "git is not installed (or not in PATH): install it to use a git knowledge source"
                .to_string()
        })?;
    if !out.status.success() {
        return Err("git --version failed".into());
    }
    Ok("git".to_string())
}

async fn run(git: &str, args: &[&str]) -> Result<(), String> {
    let out = tokio::process::Command::new(git)
        .args(args)
        .output()
        .await
        .map_err(|e| format!("git {args:?}: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(())
}

fn slug(text: &str) -> String {
    text.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect::<String>()
        .trim_matches('-')
        .chars()
        .take(40)
        .collect()
}
