//! `ka-agent import <agent> [--dry-run]`: bring memory and skills from
//! another resident agent. Memory becomes **candidates** — the operator
//! still accepts each one (the anti-pollution rule is not waived for
//! imports); skills land in the shared library, the same format.

use std::process::ExitCode;

use ka_agent_runtime::paths::Paths;

use crate::ImportAction;

fn fail(msg: impl std::fmt::Display) -> ExitCode {
    eprintln!("ka-agent: {msg}");
    ExitCode::FAILURE
}

/// Where each agent keeps what we want. `~`-relative, resolved per source.
struct Source {
    memory: Vec<(String, std::path::PathBuf)>,
    skills: std::path::PathBuf,
}

fn sources(home: &std::path::Path) -> Vec<(&'static str, Source)> {
    let h = home.to_path_buf();
    vec![
        (
            "hermes",
            Source {
                memory: vec![
                    (
                        "hermes/MEMORY.md".into(),
                        h.join(".hermes/memories/MEMORY.md"),
                    ),
                    ("hermes/USER.md".into(), h.join(".hermes/memories/USER.md")),
                ],
                skills: h.join(".hermes/skills"),
            },
        ),
        (
            "openclaw",
            Source {
                memory: vec![
                    (
                        "openclaw/MEMORY.md".into(),
                        h.join(".openclaw/workspace/MEMORY.md"),
                    ),
                    (
                        "openclaw/USER.md".into(),
                        h.join(".openclaw/workspace/USER.md"),
                    ),
                ],
                skills: h.join(".openclaw/skills"),
            },
        ),
        (
            "claude",
            Source {
                memory: vec![("claude/CLAUDE.md".into(), h.join(".claude/CLAUDE.md"))],
                skills: h.join(".claude/skills"),
            },
        ),
        (
            "codex",
            Source {
                memory: vec![(
                    "codex/memory_summary.md".into(),
                    h.join(".codex/memories/memory_summary.md"),
                )],
                skills: h.join(".codex/skills"),
            },
        ),
    ]
}

fn home() -> std::path::PathBuf {
    std::env::var_os("HOME")
        .filter(|h| !h.is_empty())
        .map(std::path::PathBuf::from)
        .unwrap_or_default()
}

/// Markdown lines that look like facts ("- x" bullets, plain lines),
/// without the file's own headers.
fn fact_lines(md: &str) -> Vec<String> {
    md.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .filter(|l| !l.starts_with('#'))
        .map(|l| l.trim_start_matches("- ").trim().to_string())
        .filter(|l| l.len() >= 3 && l.len() <= 300)
        .collect()
}

pub fn import(action: ImportAction) -> ExitCode {
    let agent = match &action {
        ImportAction::Agent { agent, .. } => agent.clone(),
    };
    let dry_run = match &action {
        ImportAction::Agent { dry_run, .. } => *dry_run,
    };
    let home = home();
    let Some((name, src)) = sources(&home).into_iter().find(|(n, _)| *n == agent) else {
        return fail(format!(
            "unknown agent {agent:?} — hermes | openclaw | claude | codex"
        ));
    };
    let _ = name;

    // Memory → candidates.
    let mut candidates: Vec<String> = Vec::new();
    for (label, path) in &src.memory {
        let Ok(md) = std::fs::read_to_string(path) else {
            continue;
        };
        for fact in fact_lines(&md) {
            candidates.push(fact);
        }
        println!("memory: {} fact(s) from {label}", fact_lines(&md).len());
    }
    if candidates.is_empty() {
        println!("memory: nothing found for {agent}");
    }

    // Skills → the shared library (a directory holding SKILL.md files).
    let mut skills: Vec<String> = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&src.skills) {
        for e in entries.flatten() {
            if e.path().join("SKILL.md").is_file()
                && let Some(name) = e.file_name().to_str()
                && ka_agent_core::layout::valid_profile_name(name)
            {
                skills.push(name.to_string());
            }
        }
    }
    skills.sort();
    if skills.is_empty() {
        println!("skills: nothing found for {agent}");
    } else {
        println!("skills: {} ({})", skills.len(), skills.join(", "));
    }

    if dry_run {
        println!("dry run — nothing was written");
        for c in candidates.iter().take(5) {
            println!("  candidate: {c}");
        }
        if candidates.len() > 5 {
            println!("  … and {} more", candidates.len() - 5);
        }
        return ExitCode::SUCCESS;
    }

    let paths = Paths::new(Paths::default_root());
    if let Err(e) = paths.ensure_dirs() {
        return fail(format!("{}: {e}", paths.root().display()));
    }
    let store = match ka_agent_store::Store::open(&paths.state_db()) {
        Ok(s) => s,
        Err(e) => return fail(format!("{}: {e}", paths.state_db().display())),
    };
    for fact in &candidates {
        let id = ka_agent_core::MemoryId::generate();
        if let Err(e) = store.add_candidate(&id, "import", fact, Some(&agent)) {
            return fail(e);
        }
    }
    let lib = ka_agent_core::layout::base_dir().join("skills");
    for name in &skills {
        let from = src.skills.join(name);
        let to = lib.join(name);
        if to.exists() {
            println!("skill {name}: already in the library, left as is");
            continue;
        }
        if let Err(e) = copy_dir(&from, &to) {
            return fail(format!("skill {name}: {e}"));
        }
    }
    println!(
        "imported: {} memory candidate(s) (review: /memory review), {} skill(s)",
        candidates.len(),
        skills.len()
    );
    ExitCode::SUCCESS
}

fn copy_dir(from: &std::path::Path, to: &std::path::Path) -> Result<(), String> {
    std::fs::create_dir_all(to).map_err(|e| format!("{}: {e}", to.display()))?;
    for entry in std::fs::read_dir(from).map_err(|e| format!("{}: {e}", from.display()))? {
        let entry = entry.map_err(|e| e.to_string())?;
        let target = to.join(entry.file_name());
        if entry.path().is_dir() {
            copy_dir(&entry.path(), &target)?;
        } else {
            std::fs::copy(entry.path(), &target)
                .map_err(|e| format!("{}: {e}", target.display()))?;
        }
    }
    Ok(())
}
