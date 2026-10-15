//! The MCP bridge (`ka-agent __mcp`): a stdio MCP server the Ka engine
//! spawns as `kagent`, exposing Ka Agent's first-party tools (SEED.md §6).
//!
//! Wire format: newline-delimited JSON-RPC 2.0 (MCP 2025-06-18, matching
//! ka's client — `crates/ka-engine/src/mcp.rs` in the ka-harness repo).
//! The server opens the store directly (SQLite WAL allows concurrent
//! access with the daemon).

use std::io::Write;
use std::path::PathBuf;

use serde_json::{Value, json};

/// Tool definition: name, description, JSON schema, effect class.
struct ToolDef {
    name: &'static str,
    description: &'static str,
    schema: Value,
}

fn tools() -> Vec<ToolDef> {
    vec![
        ToolDef {
            name: "memory_propose",
            description: "Propose a durable memory about the operator for their review. Keep it atomic (one fact, <=300 chars), state it in third person, and never include secrets. The operator reviews and accepts/rejects proposals; nothing is stored until they do.",
            schema: json!({
                "type": "object",
                "properties": {
                    "text": {"type": "string", "description": "The memory fact, third person, atomic"}
                },
                "required": ["text"]
            }),
        },
        ToolDef {
            name: "notes_search",
            description: "Search the operator's local markdown notes under the notes/ directory. Returns matching lines with file paths.",
            schema: json!({
                "type": "object",
                "properties": {
                    "query": {"type": "string", "description": "Search text (case-insensitive substring match)"}
                },
                "required": ["query"]
            }),
        },
        ToolDef {
            name: "notes_append",
            description: "Append a line to a note file in the notes/ directory. Creates the file if it does not exist. File names: [a-z0-9-] only.",
            schema: json!({
                "type": "object",
                "properties": {
                    "file": {"type": "string", "description": "Note file name without extension (e.g. 'ideas', 'reading-list')"},
                    "text": {"type": "string", "description": "The line to append"}
                },
                "required": ["file", "text"]
            }),
        },
        ToolDef {
            name: "notes_read",
            description: "Read a note file from the notes/ directory.",
            schema: json!({
                "type": "object",
                "properties": {
                    "file": {"type": "string", "description": "Note file name without extension"}
                },
                "required": ["file"]
            }),
        },
    ]
}

fn data_root() -> Result<PathBuf, String> {
    std::env::var_os("KA_AGENT_DATA_DIR")
        .map(PathBuf::from)
        .ok_or_else(|| "KA_AGENT_DATA_DIR not set (spawned outside the worker env)".to_string())
}

fn notes_dir() -> Result<PathBuf, String> {
    let dir = data_root()?.join("notes");
    std::fs::create_dir_all(&dir).map_err(|e| format!("notes dir: {e}"))?;
    Ok(dir)
}

fn sanitize_note_name(name: &str) -> Result<String, String> {
    if name.is_empty()
        || name.len() > 64
        || !name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
    {
        return Err(format!(
            "bad note name {name:?}: [a-z0-9-] only, max 64 chars"
        ));
    }
    Ok(name.to_string())
}

fn handle_tool_call(name: &str, args: &Value) -> Result<String, String> {
    match name {
        "memory_propose" => {
            let text = args["text"].as_str().ok_or("missing 'text'")?;
            if text.is_empty() || text.len() > 300 {
                return Err("text must be 1-300 chars".into());
            }
            let root = data_root()?;
            let store = ka_agent_store::Store::open(&root.join("state.db"))
                .map_err(|e| format!("store: {e}"))?;
            let id = ka_agent_core::MemoryId::generate();
            store
                .add_candidate(&id, "model", text, None)
                .map_err(|e| format!("candidate: {e}"))?;
            Ok(format!(
                "proposed for review: {id} (the operator will see it in /memory review)"
            ))
        }
        "notes_search" => {
            let query = args["query"].as_str().ok_or("missing 'query'")?;
            let dir = notes_dir()?;
            let lower = query.to_lowercase();
            let mut hits = Vec::new();
            if let Ok(entries) = std::fs::read_dir(&dir) {
                for entry in entries.flatten() {
                    let path = entry.path();
                    if path.extension().is_some_and(|e| e == "md") {
                        if let Ok(content) = std::fs::read_to_string(&path) {
                            for (i, line) in content.lines().enumerate() {
                                if line.to_lowercase().contains(&lower) {
                                    let fname =
                                        path.file_stem().and_then(|s| s.to_str()).unwrap_or("?");
                                    hits.push(format!("{fname}:{i}: {line}"));
                                    if hits.len() >= 20 {
                                        break;
                                    }
                                }
                            }
                        }
                    }
                    if hits.len() >= 20 {
                        break;
                    }
                }
            }
            if hits.is_empty() {
                Ok(format!("no matches for {query:?}"))
            } else {
                Ok(hits.join("\n"))
            }
        }
        "notes_append" => {
            let file = args["file"].as_str().ok_or("missing 'file'")?;
            let text = args["text"].as_str().ok_or("missing 'text'")?;
            let name = sanitize_note_name(file)?;
            let dir = notes_dir()?;
            let path = dir.join(format!("{name}.md"));
            let mut content = std::fs::read_to_string(&path).unwrap_or_default();
            if !content.is_empty() && !content.ends_with('\n') {
                content.push('\n');
            }
            content.push_str(text);
            content.push('\n');
            std::fs::write(&path, content).map_err(|e| format!("write: {e}"))?;
            Ok(format!("appended to {name}.md"))
        }
        "notes_read" => {
            let file = args["file"].as_str().ok_or("missing 'file'")?;
            let name = sanitize_note_name(file)?;
            let dir = notes_dir()?;
            let path = dir.join(format!("{name}.md"));
            match std::fs::read_to_string(&path) {
                Ok(content) => {
                    if content.len() > 8000 {
                        Ok(format!("{}… (truncated)", &content[..8000]))
                    } else {
                        Ok(content)
                    }
                }
                Err(_) => Ok(format!("{name}.md is empty or does not exist")),
            }
        }
        _ => Err(format!("unknown tool: {name}")),
    }
}

/// Entry point for `ka-agent __mcp`. Reads JSON-RPC from stdin, writes to
/// stdout, logs to stderr.
pub fn run_mcp() -> Result<(), String> {
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();
    let mut line = String::new();
    loop {
        line.clear();
        let n = stdin
            .read_line(&mut line)
            .map_err(|e| format!("stdin: {e}"))?;
        if n == 0 {
            break;
        }
        let msg: Value = match serde_json::from_str(line.trim()) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let id = msg.get("id").cloned();
        let method = msg["method"].as_str().unwrap_or("");
        let result = match method {
            "initialize" => json!({
                "protocolVersion": "2025-06-18",
                "capabilities": {"tools": {}},
                "serverInfo": {"name": "kagent", "version": env!("CARGO_PKG_VERSION")}
            }),
            "notifications/initialized" => {
                // notification, no response
                eprintln!("kagent: initialized");
                continue;
            }
            "tools/list" => json!({
                "tools": tools().iter().map(|t| json!({
                    "name": t.name,
                    "description": t.description,
                    "inputSchema": t.schema,
                })).collect::<Vec<_>>()
            }),
            "tools/call" => {
                let tool = msg["params"]["name"].as_str().unwrap_or("");
                let args = &msg["params"]["arguments"];
                match handle_tool_call(tool, args) {
                    Ok(text) => json!({
                        "content": [{"type": "text", "text": text}]
                    }),
                    Err(e) => json!({
                        "isError": true,
                        "content": [{"type": "text", "text": e}]
                    }),
                }
            }
            _ => {
                json!({"error": {"code": -32601, "message": format!("method not found: {method}")}})
            }
        };
        if let Some(id) = id {
            let response = json!({"jsonrpc": "2.0", "id": id, "result": result});
            writeln!(stdout, "{response}").map_err(|e| format!("stdout: {e}"))?;
            stdout.flush().map_err(|e| format!("stdout flush: {e}"))?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[test]
    fn note_name_validation() {
        assert!(sanitize_note_name("ideas").is_ok());
        assert!(sanitize_note_name("reading-list").is_ok());
        assert!(sanitize_note_name("a").is_ok());
        assert!(sanitize_note_name("").is_err());
        assert!(sanitize_note_name(" Ideas").is_err());
        assert!(sanitize_note_name("../etc/passwd").is_err());
        assert!(sanitize_note_name(&"x".repeat(65)).is_err());
    }
}
