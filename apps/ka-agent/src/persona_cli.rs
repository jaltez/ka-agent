//! `ka-agent persona [show|edit|reset]`: the assistant's persona file
//! (`<data>/memory/persona.md`) and the compiled engine identity
//! (`<data>/engine-home/.config/ka/IDENTITY.md`). Works out of process:
//! it rewrites IDENTITY.md itself with the same compiler the runtime
//! uses, so an edit applies on the next turn without a restart.

use std::path::PathBuf;

use ka_agent_runtime::paths::Paths;

use crate::cli::PersonaAction;
use crate::prompt;

/// The persona file inside the data root.
fn persona_path(paths: &Paths) -> PathBuf {
    paths.root().join("memory/persona.md")
}

/// The compiled identity the engine reads every turn.
fn identity_path(paths: &Paths) -> PathBuf {
    paths.engine_home().join(".config/ka/IDENTITY.md")
}

/// Open the memory store (creating it on first use) so every write goes
/// through the same validation + git commit path the runtime uses.
fn store(paths: &Paths) -> Result<ka_agent_memory::MemoryStore, String> {
    let root = paths.root().join("memory");
    std::fs::create_dir_all(&root).map_err(|e| format!("{}: {e}", root.display()))?;
    ka_agent_memory::MemoryStore::open(&root).map_err(|e| e.to_string())
}

fn recompile(paths: &Paths, store: &ka_agent_memory::MemoryStore) -> Result<(), String> {
    let text = ka_agent_runtime::compile_identity(&store.persona_text());
    let path = identity_path(paths);
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    }
    std::fs::write(&path, text).map_err(|e| format!("{}: {e}", path.display()))
}

pub fn persona(action: Option<PersonaAction>, _config: PathBuf) -> std::process::ExitCode {
    let paths = Paths::new(Paths::default_root());
    let mem = match store(&paths) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("ka-agent: {e}");
            return std::process::ExitCode::FAILURE;
        }
    };
    match action.unwrap_or(PersonaAction::Show) {
        PersonaAction::Show => {
            print!("{}", mem.persona_text());
            std::process::ExitCode::SUCCESS
        }
        PersonaAction::Reset => {
            let name = mem.persona_name();
            let default = ka_agent_memory::persona::DEFAULT_PERSONA;
            // Keep the operator's chosen name; the body is the default.
            let body = default
                .split_once('\n')
                .map(|(_, rest)| rest)
                .unwrap_or(default);
            let text = format!("# {name}\n{body}");
            if let Err(e) = mem.write_persona(&text) {
                eprintln!("ka-agent: {e}");
                return std::process::ExitCode::FAILURE;
            }
            match recompile(&paths, &mem) {
                Ok(()) => {
                    println!("persona reset to the default (still {name})");
                    std::process::ExitCode::SUCCESS
                }
                Err(e) => {
                    eprintln!("ka-agent: {e}");
                    std::process::ExitCode::FAILURE
                }
            }
        }
        PersonaAction::Edit => {
            let tmp = persona_path(&paths).with_extension("edit.md");
            if std::fs::copy(persona_path(&paths), &tmp).is_err() {
                let _ = std::fs::write(&tmp, mem.persona_text());
            }
            loop {
                let editor = std::env::var("VISUAL")
                    .or_else(|_| std::env::var("EDITOR"))
                    .unwrap_or_else(|_| {
                        if cfg!(windows) {
                            "notepad".to_string()
                        } else {
                            "vi".to_string()
                        }
                    });
                match std::process::Command::new(&editor).arg(&tmp).status() {
                    Ok(s) if s.success() => {}
                    _ => {
                        eprintln!("ka-agent: editor {editor:?} failed");
                        let _ = std::fs::remove_file(&tmp);
                        return std::process::ExitCode::FAILURE;
                    }
                }
                let text = std::fs::read_to_string(&tmp).unwrap_or_default();
                match mem.write_persona(&text) {
                    Ok(()) => {
                        let _ = std::fs::remove_file(&tmp);
                        return match recompile(&paths, &mem) {
                            Ok(()) => {
                                println!(
                                    r#"persona saved — the next turn speaks as "{}""#,
                                    mem.persona_name()
                                );
                                std::process::ExitCode::SUCCESS
                            }
                            Err(e) => {
                                eprintln!("ka-agent: {e}");
                                std::process::ExitCode::FAILURE
                            }
                        };
                    }
                    Err(e) => {
                        println!("invalid persona: {e}");
                        let options = vec!["Edit again".to_string(), "Discard changes".to_string()];
                        match prompt::choice("", &options, 0) {
                            Ok(0) => continue,
                            _ => {
                                let _ = std::fs::remove_file(&tmp);
                                println!("changes discarded");
                                return std::process::ExitCode::FAILURE;
                            }
                        }
                    }
                }
            }
        }
    }
}
