//! `ka-agent mcp add|list|rm`: operator MCP servers (`[[mcp]]` in the
//! config). Values written `secret:<name>` resolve from the secret store
//! when the worker starts, so tokens never sit in the config file.

use std::path::PathBuf;
use std::process::ExitCode;

use ka_agent_core::config_file;
use ka_agent_runtime::paths::Paths;

use crate::McpAction;

fn fail(msg: impl std::fmt::Display) -> ExitCode {
    eprintln!("ka-agent: {msg}");
    ExitCode::FAILURE
}

/// `K=V` pairs from the command line.
fn pairs(raw: &[String], what: &str) -> Result<toml_edit::InlineTable, String> {
    let mut t = toml_edit::InlineTable::new();
    for kv in raw {
        let (k, v) = kv
            .split_once('=')
            .ok_or_else(|| format!("{what} {kv:?}: expected KEY=VALUE"))?;
        if k.is_empty() {
            return Err(format!("{what} {kv:?}: empty key"));
        }
        t.insert(k, v.into());
    }
    Ok(t)
}

pub fn mcp_action(action: McpAction, config: PathBuf) -> ExitCode {
    let paths = Paths::new(Paths::default_root());
    if !config.exists() {
        return fail(format!(
            "Ka Agent isn't set up yet ({} missing) — run `ka-agent setup`",
            config.display()
        ));
    }
    match action {
        McpAction::List => {
            let cfg = match ka_agent_core::DaemonConfig::load(&config) {
                Ok(c) => c,
                Err(e) => return fail(e),
            };
            if cfg.mcp.is_empty() {
                println!("no MCP servers — ka-agent mcp add <name> -- <command> [args…]");
            }
            for m in &cfg.mcp {
                let target = match (&m.command, &m.url) {
                    (Some(c), _) => format!("{c} {}", m.args.join(" ")),
                    (_, Some(u)) => u.clone(),
                    _ => "?".into(),
                };
                let secrets: Vec<&str> = m
                    .env
                    .values()
                    .chain(m.headers.values())
                    .filter_map(|v| v.strip_prefix(ka_agent_port::engine_layer::SECRET_PREFIX))
                    .collect();
                let note = if secrets.is_empty() {
                    String::new()
                } else {
                    format!("  (secrets: {})", secrets.join(", "))
                };
                println!("{:<12} {}{note}", m.name, target.trim());
            }
            ExitCode::SUCCESS
        }
        McpAction::Add {
            name,
            url,
            env,
            header,
            command,
        } => {
            if !ka_agent_core::McpServer::valid_name(&name) {
                return fail(format!(
                    "name {name:?}: [a-z0-9_-], up to 32 chars, not \"kagent\""
                ));
            }
            let mut doc = match config_file::read_doc(&config) {
                Ok(d) => d,
                Err(e) => return fail(e),
            };
            let mut row = toml_edit::Table::new();
            row.insert("name", toml_edit::value(name.clone()));
            match (url, command.split_first()) {
                (Some(url), None) => {
                    row.insert("url", toml_edit::value(url));
                }
                (None, Some((cmd, args))) => {
                    row.insert("command", toml_edit::value(cmd.clone()));
                    if !args.is_empty() {
                        let mut a = toml_edit::Array::new();
                        for x in args {
                            a.push(x.as_str());
                        }
                        row.insert("args", toml_edit::value(a));
                    }
                }
                _ => {
                    return fail(
                        "give either `--url <url>` or a command after `--` (ka-agent mcp add notes -- npx -y some-mcp)",
                    );
                }
            }
            match pairs(&env, "--env") {
                Ok(t) if !t.is_empty() => {
                    row.insert("env", toml_edit::value(t));
                }
                Ok(_) => {}
                Err(e) => return fail(e),
            }
            match pairs(&header, "--header") {
                Ok(t) if !t.is_empty() => {
                    row.insert("headers", toml_edit::value(t));
                }
                Ok(_) => {}
                Err(e) => return fail(e),
            }
            let servers = doc.entry("mcp").or_insert(toml_edit::Item::ArrayOfTables(
                toml_edit::ArrayOfTables::new(),
            ));
            let Some(servers) = servers.as_array_of_tables_mut() else {
                return fail(
                    "`mcp` in the config is not an array of tables — fix it with `ka-agent config edit`",
                );
            };
            servers.push(row);
            if let Err(e) = config_file::write_doc(&config, &doc) {
                return fail(e);
            }
            println!("added MCP server {name}");
            crate::config_cli::apply_live(&paths, &config)
        }
        McpAction::Rm { name } => {
            let mut doc = match config_file::read_doc(&config) {
                Ok(d) => d,
                Err(e) => return fail(e),
            };
            let removed = doc
                .get_mut("mcp")
                .and_then(toml_edit::Item::as_array_of_tables_mut)
                .map(|servers| {
                    let before = servers.len();
                    servers
                        .retain(|t| t.get("name").and_then(|v| v.as_str()) != Some(name.as_str()));
                    before != servers.len()
                })
                .unwrap_or(false);
            if !removed {
                return fail(format!("no MCP server {name:?} — ka-agent mcp list"));
            }
            if doc
                .get("mcp")
                .and_then(toml_edit::Item::as_array_of_tables)
                .is_some_and(|a| a.is_empty())
            {
                doc.remove("mcp");
            }
            if let Err(e) = config_file::write_doc(&config, &doc) {
                return fail(e);
            }
            println!("removed MCP server {name}");
            crate::config_cli::apply_live(&paths, &config)
        }
    }
}
