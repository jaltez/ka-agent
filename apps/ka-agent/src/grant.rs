//! Tool grants (SEED.md §8.2): explicit, revocable scopes that connector
//! tools check before any external effect — plus the config's directory
//! grants (`[[grants.path]]`), which say where the agent may work
//! without asking (D1). Two stores, one command: scopes live in
//! state.db, path grants in `ka-agent.toml`.

use std::path::Path;

use ka_agent_channels::grants::Grants;
use ka_agent_runtime::paths::Paths;
use ka_agent_store::Store;

use crate::GrantAction;

/// Directory grants (`[[grants.path]]` in the config): `grant path add
/// /proj [--access allow|ask|deny]`, `grant path rm /proj`, `grant path
/// list`. Writes through the format-preserving editor (the `mcp add`
/// pattern), so comments and neighboring keys survive.
#[derive(clap::Subcommand)]
pub(crate) enum PathGrantAction {
    /// Grant the agent access to a directory.
    Add {
        /// The directory (absolute; `~` expands).
        dir: std::path::PathBuf,
        /// allow (work without asking) | ask | deny.
        #[arg(long, default_value = "allow")]
        access: String,
    },
    /// Remove a directory grant.
    Rm { dir: std::path::PathBuf },
    /// List directory grants.
    List,
}

pub fn grant_action(action: GrantAction) -> std::process::ExitCode {
    // Path grants are config rows, not store rows: never touch
    // state.db for them (a fresh root without one still works).
    if let GrantAction::Path { action } = action {
        return path_action(action);
    }
    let paths = Paths::new(Paths::default_root());
    let store = match Store::open(&paths.state_db()) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("ka-agent: store: {e}");
            return std::process::ExitCode::FAILURE;
        }
    };
    let mut grants = match Grants::load(&store) {
        Ok(g) => g,
        Err(e) => {
            eprintln!("ka-agent: grants: {e}");
            return std::process::ExitCode::FAILURE;
        }
    };
    match action {
        GrantAction::Add { scope } => {
            grants.add(&scope);
            if let Err(e) = grants.save(&store) {
                eprintln!("ka-agent: {e}");
                return std::process::ExitCode::FAILURE;
            }
            println!("grant added: {scope}");
        }
        GrantAction::Remove { scope } => {
            if grants.remove(&scope) {
                if let Err(e) = grants.save(&store) {
                    eprintln!("ka-agent: {e}");
                    return std::process::ExitCode::FAILURE;
                }
                println!("grant removed: {scope}");
            } else {
                println!("grant not found: {scope}");
            }
        }
        GrantAction::List => {
            if grants.scopes.is_empty() {
                println!("no tool scopes");
                // D12: `grant` names two systems; the empty state must
                // not dead-end when the operator came looking for their
                // directory grants.
                println!(
                    "(directory access grants live in the config, [[grants.path]] — `ka-agent grant path list`)"
                );
            } else {
                for s in &grants.scopes {
                    println!("{s}");
                }
            }
        }
        // Handled before the store was opened; unreachable in practice.
        GrantAction::Path { .. } => {}
    }
    std::process::ExitCode::SUCCESS
}
fn fail(msg: impl std::fmt::Display) -> std::process::ExitCode {
    eprintln!("ka-agent: {msg}");
    std::process::ExitCode::FAILURE
}

/// `dir` with `~` expanded, refused when it is not absolute — the same
/// rule `DaemonConfig::validate` applies, caught before any write.
fn absolute_dir(dir: &Path) -> Result<std::path::PathBuf, String> {
    let dir = ka_agent_core::expand_home(&dir.display().to_string());
    if !dir.is_absolute() {
        return Err(format!(
            "grants.path dir {} must be absolute",
            dir.display()
        ));
    }
    Ok(dir)
}

fn path_action(action: PathGrantAction) -> std::process::ExitCode {
    let config = ka_agent_core::layout::default_config_path();
    if !config.exists() {
        return fail(format!(
            "Ka Agent isn't set up yet ({} missing) — run `ka-agent setup`",
            config.display()
        ));
    }
    match action {
        PathGrantAction::Add { dir, access } => {
            let access = match access.as_str() {
                "allow" | "always" => "allow",
                "ask" => "ask",
                "deny" => "deny",
                other => return fail(format!("access {other:?}: allow | ask | deny")),
            };
            let dir = match absolute_dir(&dir) {
                Ok(d) => d,
                Err(e) => return fail(e),
            };
            let mut doc = match ka_agent_core::config_file::read_doc(&config) {
                Ok(d) => d,
                Err(e) => return fail(e),
            };
            let grants = doc
                .entry("grants")
                .or_insert(toml_edit::Item::Table(toml_edit::Table::new()));
            let Some(grants) = grants.as_table_like_mut() else {
                return fail(
                    "`grants` in the config is not a table — fix it with `ka-agent config edit`",
                );
            };
            let rows = grants
                .entry("path")
                .or_insert(toml_edit::Item::ArrayOfTables(
                    toml_edit::ArrayOfTables::new(),
                ));
            let Some(rows) = rows.as_array_of_tables_mut() else {
                return fail(
                    "`grants.path` in the config is not an array of tables — fix it with `ka-agent config edit`",
                );
            };
            // A second grant for the same directory would fail
            // validation (`listed twice`): re-aim the existing row.
            let dir_text = dir.display().to_string();
            let mut existing = false;
            for row in rows.iter_mut() {
                let same = row
                    .get("dir")
                    .and_then(|v| v.as_str())
                    .is_some_and(|s| Path::new(s) == dir.as_path());
                if same {
                    row.insert("dir", toml_edit::value(dir_text.clone()));
                    row.insert("access", toml_edit::value(access));
                    existing = true;
                }
            }
            if !existing {
                let mut row = toml_edit::Table::new();
                row.insert("dir", toml_edit::value(dir_text.clone()));
                row.insert("access", toml_edit::value(access));
                rows.push(row);
            }
            if let Err(e) = ka_agent_core::config_file::write_doc(&config, &doc) {
                return fail(e);
            }
            if existing {
                println!("path grant for {} now {access}", dir_text);
            } else {
                println!("path grant {access} {}", dir_text);
            }
            crate::config_cli::apply_live(&config)
        }
        PathGrantAction::Rm { dir } => {
            let dir = match absolute_dir(&dir) {
                Ok(d) => d,
                Err(e) => return fail(e),
            };
            let mut doc = match ka_agent_core::config_file::read_doc(&config) {
                Ok(d) => d,
                Err(e) => return fail(e),
            };
            let removed = doc
                .get_mut("grants")
                .and_then(toml_edit::Item::as_table_like_mut)
                .and_then(|grants| grants.get_mut("path"))
                .and_then(toml_edit::Item::as_array_of_tables_mut)
                .map(|rows| {
                    let before = rows.len();
                    rows.retain(|t| {
                        t.get("dir")
                            .and_then(|v| v.as_str())
                            .is_none_or(|s| Path::new(s) != dir.as_path())
                    });
                    before != rows.len()
                })
                .unwrap_or(false);
            if !removed {
                return fail(format!(
                    "no path grant for {} — ka-agent grant path list",
                    dir.display()
                ));
            }
            // Drop the emptied array (and an emptied [grants] table with
            // it) instead of leaving dead headers behind.
            if let Some(grants) = doc
                .get_mut("grants")
                .and_then(toml_edit::Item::as_table_like_mut)
            {
                let empty = grants
                    .get("path")
                    .and_then(toml_edit::Item::as_array_of_tables)
                    .is_some_and(|a| a.is_empty());
                if empty {
                    grants.remove("path");
                }
                if grants.iter().next().is_none() {
                    doc.remove("grants");
                }
            }
            if let Err(e) = ka_agent_core::config_file::write_doc(&config, &doc) {
                return fail(e);
            }
            println!("removed path grant {}", dir.display());
            crate::config_cli::apply_live(&config)
        }
        PathGrantAction::List => {
            let cfg = match ka_agent_core::DaemonConfig::load(&config) {
                Ok(c) => c,
                Err(e) => return fail(e),
            };
            if cfg.grants.path.is_empty() {
                println!(
                    "no path grants — ka-agent grant path add <dir> [--access allow|ask|deny]"
                );
            }
            for g in &cfg.grants.path {
                println!("{:<5} {}", access_word(g.access), g.dir.display());
            }
            std::process::ExitCode::SUCCESS
        }
    }
}

fn access_word(a: ka_agent_core::config::Access) -> &'static str {
    match a {
        ka_agent_core::config::Access::Allow => "allow",
        ka_agent_core::config::Access::Ask => "ask",
        ka_agent_core::config::Access::Deny => "deny",
    }
}
