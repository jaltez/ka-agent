//! `ka-agent memory …`: CLI access to the stable-memory files. The files
//! are plain Markdown the operator owns; this only adds the editor loop
//! (SEED.md §9.1 `ka-agent memory edit`) and read-only views, and keeps
//! the compiled `MEMORY.md` in sync after every edit.

use ka_agent_memory::{MemoryFile, MemoryStore};
use ka_agent_runtime::paths::Paths;

use crate::MemoryAction;

fn resolve_file(name: &str) -> Option<MemoryFile> {
    match name {
        "" | "facts" => Some(MemoryFile::Facts),
        "profile" => Some(MemoryFile::Profile),
        "preferences" => Some(MemoryFile::Preferences),
        "environment" => Some(MemoryFile::Environment),
        "conventions" => Some(MemoryFile::Conventions),
        _ => None,
    }
}

fn open_store() -> Result<MemoryStore, ka_agent_memory::Error> {
    let paths = Paths::new(Paths::default_root());
    MemoryStore::open(&paths.root().join("memory"))
}

fn engine_config_ka(paths: &Paths) -> std::path::PathBuf {
    paths.engine_home().join(".config/ka")
}

/// After an operator edit: recompile into the engine's MEMORY.md and let
/// git record what changed. The engine re-reads MEMORY.md per prompt, so
/// no restart is needed.
fn resync(store: &MemoryStore, file: &MemoryFile) -> Result<(), String> {
    let paths = Paths::new(Paths::default_root());
    let report = store
        .install_compiled(
            &engine_config_ka(&paths),
            ka_agent_memory::DEFAULT_BUDGET_BYTES,
            "",
        )
        .map_err(|e| e.to_string())?;
    let changed = store
        .commit(&format!("memory: edit {} via CLI", file.path().display()))
        .map_err(|e| e.to_string())?;
    let _ = changed;
    let entries = store.entries().map_err(|e| e.to_string())?.len();
    println!(
        "recompiled {}/{} bytes{} · {entries} entries with provenance",
        report.total_bytes,
        report.budget_bytes,
        if report.truncated { " (truncated)" } else { "" }
    );
    Ok(())
}

pub fn memory_action(action: MemoryAction) -> std::process::ExitCode {
    match action {
        MemoryAction::Path => {
            let paths = Paths::new(Paths::default_root());
            println!("{}", paths.root().join("memory").display());
            std::process::ExitCode::SUCCESS
        }
        MemoryAction::Show { file } => {
            let Some(f) = resolve_file(&file.as_deref().unwrap_or("").to_ascii_lowercase()) else {
                eprintln!(
                    "ka-agent: file {file:?}: facts | profile | persona | preferences | environment | conventions"
                );
                return std::process::ExitCode::FAILURE;
            };
            match open_store().and_then(|s| s.file_text(&f)) {
                Ok(Some(text)) => {
                    print!("{text}");
                    std::process::ExitCode::SUCCESS
                }
                Ok(None) => {
                    eprintln!("ka-agent: no such memory file");
                    std::process::ExitCode::FAILURE
                }
                Err(e) => {
                    eprintln!("ka-agent: {e}");
                    std::process::ExitCode::FAILURE
                }
            }
        }
        MemoryAction::Edit { file } => {
            let Some(f) = resolve_file(&file.as_deref().unwrap_or("").to_ascii_lowercase()) else {
                eprintln!(
                    "ka-agent: file {file:?}: facts | profile | persona | preferences | environment | conventions"
                );
                return std::process::ExitCode::FAILURE;
            };
            let store = match open_store() {
                Ok(s) => s,
                Err(e) => {
                    eprintln!("ka-agent: {e}");
                    return std::process::ExitCode::FAILURE;
                }
            };
            let path = store.root().join(f.path());
            let editor = std::env::var("EDITOR")
                .unwrap_or_else(|_| std::env::var("VISUAL").unwrap_or_else(|_| "vi".into()));
            let status = match std::process::Command::new(&editor).arg(&path).status() {
                Ok(s) => s,
                Err(e) => {
                    eprintln!("ka-agent: cannot run {editor:?}: {e}");
                    return std::process::ExitCode::FAILURE;
                }
            };
            if !status.success() {
                eprintln!("ka-agent: editor exited with {status}; nothing recompiled");
                return std::process::ExitCode::FAILURE;
            }
            match resync(&store, &f) {
                Ok(()) => std::process::ExitCode::SUCCESS,
                Err(e) => {
                    eprintln!("ka-agent: {e}");
                    std::process::ExitCode::FAILURE
                }
            }
        }
    }
}
