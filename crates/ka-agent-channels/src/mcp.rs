//! The MCP bridge (`ka-agent __mcp`): a stdio MCP server the Ka engine
//! spawns as `kagent`, exposing Ka Agent's first-party tools (SEED.md §6).
//!
//! Wire format: newline-delimited JSON-RPC 2.0 (MCP 2025-06-18, matching
//! ka's client — `crates/ka-engine/src/mcp.rs` in the ka-harness repo).
//! The server opens the store directly (SQLite WAL allows concurrent
//! access with the daemon).

use crate::grants;
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

fn handle_tool_call(name: &str, args: &Value, grants: &grants::Grants) -> Result<String, String> {
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
        "http_get" => {
            let url = args["url"].as_str().ok_or("missing 'url'")?;
            let parsed = url::Url::parse(url).map_err(|e| format!("bad url: {e}"))?;
            if parsed.scheme() != "http" && parsed.scheme() != "https" {
                return Err(format!("unsupported scheme: {}", parsed.scheme()));
            }
            let host = parsed.host_str().ok_or("missing host")?.to_string();
            let scope = format!("http:get:{host}");
            if !grants.check(&scope) {
                return Err(format!(
                    "no grant for {scope} — ask the operator to run: ka-agent grant add {scope}"
                ));
            }
            // SSRF: refuse private/loopback IPs unless explicitly granted.
            if !grants.check("http:get:private")
                && (host == "127.0.0.1"
                    || host == "localhost"
                    || host.starts_with("10.")
                    || host.starts_with("192.168.")
                    || host.starts_with("172."))
            {
                return Err("private/loopback hosts require the http:get:private grant".into());
            }
            let body = http_get_sync(&parsed, 15)?;
            if body.len() > 200_000 {
                Ok(format!("{}… (truncated at 200 KB)", &body[..200_000]))
            } else {
                Ok(body)
            }
        }
        "github_issues_list" => {
            let repo = args["repo"].as_str().ok_or("missing 'repo'")?;
            let state = args["state"].as_str().unwrap_or("open");
            let limit = args["limit"].as_u64().unwrap_or(10).min(30) as usize;
            let scope = "github:issues:read";
            if !grants.check(scope) {
                return Err(format!(
                    "no grant for {scope} — ask the operator to run: ka-agent grant add {scope}"
                ));
            }
            let token = std::env::var("KA_AGENT_GITHUB_TOKEN")
                .map_err(|_| "no GitHub token: set KA_AGENT_GITHUB_TOKEN env".to_string())?;
            let body = github_api(
                &token,
                &format!("/repos/{repo}/issues?state={state}&per_page={limit}"),
            )?;
            let issues = body.as_array().ok_or("unexpected GitHub response")?;
            let lines: Vec<String> = issues
                .iter()
                .filter_map(|i| {
                    let num = i["number"].as_u64()?;
                    let title = i["title"].as_str()?;
                    let state = i["state"].as_str()?;
                    Some(format!("#{num} [{state}] {title}"))
                })
                .collect();
            if lines.is_empty() {
                Ok(format!("no {state} issues in {repo}"))
            } else {
                Ok(lines.join("\n"))
            }
        }
        "github_pulls_get" => {
            let repo = args["repo"].as_str().ok_or("missing 'repo'")?;
            let number = args["number"].as_u64().ok_or("missing 'number'")?;
            let scope = "github:pulls:read";
            if !grants.check(scope) {
                return Err(format!(
                    "no grant for {scope} — ask the operator to run: ka-agent grant add {scope}"
                ));
            }
            let token = std::env::var("KA_AGENT_GITHUB_TOKEN")
                .map_err(|_| "no GitHub token: set KA_AGENT_GITHUB_TOKEN env".to_string())?;
            let body = github_api(&token, &format!("/repos/{repo}/pulls/{number}"))?;
            let title = body["title"].as_str().unwrap_or("?");
            let state = body["state"].as_str().unwrap_or("?");
            let user = body["user"]["login"].as_str().unwrap_or("?");
            let body_text = body["body"].as_str().unwrap_or("");
            Ok(format!(
                "#{number} [{state}] {title}\nby {user}\n{body_text}"
            ))
        }
        _ => Err(format!("unknown tool: {name}")),
    }
}

/// Blocking HTTP GET with a 15 s timeout, returns the response body as text.
fn http_get_sync(url: &url::Url, timeout_secs: u64) -> Result<String, String> {
    let agent = ureq::AgentBuilder::new()
        .timeout(std::time::Duration::from_secs(timeout_secs))
        .build();
    let resp = agent
        .get(url.as_str())
        .call()
        .map_err(|e| format!("http: {e}"))?;
    let content_type = resp.header("content-type").unwrap_or("").to_string();
    if !content_type.starts_with("text/")
        && !content_type.starts_with("application/json")
        && !content_type.starts_with("application/xml")
    {
        return Err(format!("unsupported content-type: {content_type}"));
    }
    resp.into_string().map_err(|e| format!("read body: {e}"))
}

/// Simple GitHub API GET using ureq.
fn github_api(token: &str, path: &str) -> Result<Value, String> {
    let agent = ureq::AgentBuilder::new()
        .timeout(std::time::Duration::from_secs(15))
        .build();
    let url = format!("https://api.github.com{path}");
    let response = agent
        .get(&url)
        .set("Authorization", &format!("Bearer {token}"))
        .set("Accept", "application/vnd.github+json")
        .set("User-Agent", "ka-agent")
        .call()
        .map_err(|e| format!("github: {e}"))?;
    let text = response
        .into_string()
        .map_err(|e| format!("github body: {e}"))?;
    let body: Value = serde_json::from_str(&text).map_err(|e| format!("github json: {e}"))?;
    Ok(body)
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
                // Load grants fresh on every call (the daemon may change them).
                let grants_result = grants::open_store()
                    .and_then(|store| grants::Grants::load(&store))
                    .map_err(|e| format!("grants: {e}"));
                match grants_result {
                    Err(e) => {
                        let resp =
                            json!({"isError": true, "content": [{"type": "text", "text": e}]});
                        if let Some(id) = id.clone() {
                            let response = json!({"jsonrpc": "2.0", "id": id, "result": resp});
                            let _ = writeln!(stdout, "{response}");
                            stdout.flush().ok();
                        }
                        continue;
                    }
                    Ok(grants) => match handle_tool_call(tool, args, &grants) {
                        Ok(text) => json!({
                            "content": [{"type": "text", "text": text}]
                        }),
                        Err(e) => json!({
                            "isError": true,
                            "content": [{"type": "text", "text": e}]
                        }),
                    },
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
