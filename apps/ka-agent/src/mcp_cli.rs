//! `ka-agent mcp add|list|rm|test`: operator MCP servers (`[[mcp]]` in
//! the config). Values written `secret:<name>` resolve from the secret
//! store when the worker starts, so tokens never sit in the config
//! file. `mcp test` proves a server answers (initialize + tools/list)
//! at setup time, not mid-conversation.

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
            crate::config_cli::apply_live(&config)
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
            crate::config_cli::apply_live(&config)
        }
        McpAction::Test { name } => {
            let cfg = match ka_agent_core::DaemonConfig::load(&config) {
                Ok(c) => c,
                Err(e) => return fail(e),
            };
            let servers: Vec<&ka_agent_core::McpServer> = match name.as_deref() {
                Some(n) => match cfg.mcp.iter().find(|m| m.name == n) {
                    Some(m) => vec![m],
                    None => {
                        return fail(format!("no MCP server {n:?} — ka-agent mcp list"));
                    }
                },
                None => cfg.mcp.iter().collect(),
            };
            if servers.is_empty() {
                println!("no MCP servers — ka-agent mcp add <name> -- <command> [args…]");
                return ExitCode::SUCCESS;
            }
            let mut failed = false;
            for m in servers {
                match probe(m, &paths) {
                    Ok(count) => println!("{:<12} ok — {count} tool(s)", m.name),
                    Err(e) => {
                        eprintln!("ka-agent: mcp {} FAILED: {e}", m.name);
                        failed = true;
                    }
                }
            }
            if failed {
                ExitCode::FAILURE
            } else {
                ExitCode::SUCCESS
            }
        }
    }
}

// ---------------------------------------------------------------- probe

/// MCP protocol version the probe speaks (2024-11-05 — the baseline
/// every server accepts).
const PROTOCOL_VERSION: &str = "2024-11-05";
/// Per-response wait. Generous on purpose: `npx -y …` stdio servers
/// download their package on first spawn.
const RPC_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// One server probe (D4): initialize, then `tools/list`, and report the
/// tool count. stdio servers are spawned; HTTP servers get two POSTs.
/// The worker's own MCP client lives inside the engine, so this is a
/// minimal JSON-RPC handshake of our own — enough to prove the server
/// answers and serves tools, which is all setup-time needs.
fn probe(m: &ka_agent_core::McpServer, paths: &Paths) -> Result<usize, String> {
    match (&m.command, &m.url) {
        (Some(command), _) => probe_stdio(m, command, paths),
        (None, Some(url)) => probe_http(m, url, paths),
        _ => Err("configured without a command or a url".into()),
    }
}

/// `secret:<name>` values resolved from the secret store, exactly as the
/// worker resolves them at spawn time.
fn resolve_secrets(
    values: &std::collections::BTreeMap<String, String>,
    paths: &Paths,
) -> Result<Vec<(String, String)>, String> {
    let secrets = paths.root().join("secrets");
    values
        .iter()
        .map(
            |(k, v)| match v.strip_prefix(ka_agent_port::engine_layer::SECRET_PREFIX) {
                Some(name) => ka_agent_core::secret::resolve(name, &secrets)
                    .map(|resolved| (k.clone(), resolved))
                    .map_err(|e| format!("secret {name:?}: {e} — `ka-agent secret set {name}`")),
                None => Ok((k.clone(), v.clone())),
            },
        )
        .collect()
}

fn initialize_request() -> serde_json::Value {
    serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "protocolVersion": PROTOCOL_VERSION,
            "capabilities": {},
            "clientInfo": { "name": "ka-agent", "version": env!("CARGO_PKG_VERSION") },
        },
    })
}

fn request(id: i64, method: &str) -> serde_json::Value {
    serde_json::json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": method,
    })
}

/// The `result` (or the error message) of the JSON-RPC response with
/// this id, from a body that is either one JSON document or an SSE
/// stream of `data:` lines. `None` when the id never answers (a
/// notification or a log line — callers skip those).
fn response_for(body: &str, id: i64) -> Option<Result<serde_json::Value, String>> {
    for candidate in body_lines(body) {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&candidate) else {
            continue;
        };
        if v.get("id").and_then(serde_json::Value::as_i64) != Some(id) {
            continue;
        }
        return Some(match v.get("error") {
            Some(err) => Err(err
                .get("message")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("unknown error")
                .to_string()),
            None => Ok(v.get("result").cloned().unwrap_or(serde_json::Value::Null)),
        });
    }
    None
}

/// The JSON candidates in a response body: the whole text when it is
/// one document, each `data:` line when it is `text/event-stream`.
fn body_lines(body: &str) -> Vec<String> {
    if body.contains("data:") {
        body.lines()
            .filter_map(|l| l.strip_prefix("data:"))
            .map(str::trim)
            .map(str::to_string)
            .collect()
    } else {
        vec![body.trim().to_string()]
    }
}

fn tool_count(result: &serde_json::Value) -> usize {
    result
        .get("tools")
        .and_then(serde_json::Value::as_array)
        .map_or(0, Vec::len)
}

fn probe_stdio(
    m: &ka_agent_core::McpServer,
    command: &str,
    paths: &Paths,
) -> Result<usize, String> {
    use std::io::Write;
    let mut child = std::process::Command::new(command)
        .args(&m.args)
        .envs(resolve_secrets(&m.env, paths)?)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map_err(|e| format!("spawn {command:?}: {e}"))?;
    // Reader thread: lines from the server's stdout, so a hung one
    // cannot hang the CLI (recv_timeout below gives up).
    let (tx, rx) = std::sync::mpsc::channel::<String>();
    if let Some(out) = child.stdout.take() {
        std::thread::spawn(move || {
            use std::io::BufRead;
            for line in std::io::BufReader::new(out).lines() {
                if tx.send(line.unwrap_or_default()).is_err() {
                    break;
                }
            }
        });
    }
    let result = (|| -> Result<usize, String> {
        let mut stdin = child
            .stdin
            .take()
            .ok_or("cannot write to the server's stdin")?;
        let mut send = |v: &serde_json::Value| -> Result<(), String> {
            let mut line =
                serde_json::to_string(v).map_err(|e| format!("encoding request: {e}"))?;
            line.push('\n');
            stdin
                .write_all(line.as_bytes())
                .map_err(|e| format!("writing to the server: {e}"))
        };
        send(&initialize_request())?;
        let init = wait_response(&rx, 1, "initialize")?;
        let _ = init.get("serverInfo"); // present on every conforming server
        send(&serde_json::json!({
            "jsonrpc": "2.0",
            "method": "notifications/initialized",
        }))?;
        send(&request(2, "tools/list"))?;
        let tools = wait_response(&rx, 2, "tools/list")?;
        Ok(tool_count(&tools))
    })();
    let _ = child.kill();
    let _ = child.wait();
    result
}

/// The response with `id`, skipping notifications and log lines, with a
/// timeout so a hung server fails the probe instead of the terminal.
fn wait_response(
    rx: &std::sync::mpsc::Receiver<String>,
    id: i64,
    what: &str,
) -> Result<serde_json::Value, String> {
    let deadline = std::time::Instant::now() + RPC_TIMEOUT;
    loop {
        let wait = deadline.saturating_duration_since(std::time::Instant::now());
        match rx.recv_timeout(wait) {
            Ok(line) => {
                if let Some(result) = response_for(&line, id) {
                    return result.map_err(|e| format!("{what}: {e}"));
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                return Err(format!(
                    "{what}: no answer within {} s",
                    RPC_TIMEOUT.as_secs()
                ));
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                return Err(format!(
                    "{what}: the server closed its stdout before answering"
                ));
            }
        }
    }
}

fn probe_http(m: &ka_agent_core::McpServer, url: &str, paths: &Paths) -> Result<usize, String> {
    let client = reqwest::blocking::Client::builder()
        .timeout(RPC_TIMEOUT)
        .build()
        .map_err(|e| format!("HTTP client: {e}"))?;
    let mut headers = reqwest::header::HeaderMap::new();
    for (k, v) in resolve_secrets(&m.headers, paths)? {
        let name = reqwest::header::HeaderName::try_from(k.as_str())
            .map_err(|e| format!("header {k:?}: {e}"))?;
        let value = reqwest::header::HeaderValue::try_from(v.as_str())
            .map_err(|e| format!("header {k:?} value: {e}"))?;
        headers.insert(name, value);
    }
    let post = |body: &serde_json::Value,
                headers: &reqwest::header::HeaderMap|
     -> Result<(reqwest::header::HeaderMap, String), String> {
        let resp = client
            .post(url)
            .headers(headers.clone())
            .json(body)
            .send()
            .map_err(|e| format!("HTTP: {e}"))?;
        let status = resp.status();
        let resp_headers = resp.headers().clone();
        let text = resp
            .text()
            .map_err(|e| format!("reading the response: {e}"))?;
        if !status.is_success() {
            return Err(format!("HTTP {} from {}", status, url));
        }
        Ok((resp_headers, text))
    };
    let (resp_headers, text) = post(&initialize_request(), &headers)?;
    let session = resp_headers
        .get("mcp-session-id")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    if let Some(session) = &session
        && let Ok(value) = reqwest::header::HeaderValue::try_from(session.as_str())
    {
        headers.insert("mcp-session-id", value);
    }
    response_for(&text, 1)
        .ok_or_else(|| "initialize: no JSON-RPC answer in the response".to_string())??;
    // The initialized notification: some servers refuse tools/list
    // without it; its own response (often 202, empty) is irrelevant.
    let _ = post(
        &serde_json::json!({
            "jsonrpc": "2.0",
            "method": "notifications/initialized",
        }),
        &headers,
    );
    let (_, text) = post(&request(2, "tools/list"), &headers)?;
    let tools = response_for(&text, 2)
        .ok_or_else(|| "tools/list: no JSON-RPC answer in the response".to_string())??;
    Ok(tool_count(&tools))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    #[test]
    fn json_and_sse_bodies_both_yield_the_matching_response() {
        let init = r#"{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2024-11-05"}}"#;
        assert!(response_for(init, 1).unwrap().is_ok());
        assert!(response_for(init, 2).is_none());
        // A notification (no id) never matches.
        assert!(response_for(r#"{"jsonrpc":"2.0","method":"logging/message"}"#, 1).is_none());
        // SSE: data lines, possibly interleaved with events.
        let sse = "event: message\ndata: {\"jsonrpc\":\"2.0\",\"id\":2,\"result\":{\"tools\":[{\"name\":\"echo\"},{\"name\":\"note\"}]}}\n\n";
        let tools = response_for(sse, 2).unwrap().unwrap();
        assert_eq!(tool_count(&tools), 2);
    }

    #[test]
    fn error_responses_carry_the_message() {
        let err = r#"{"jsonrpc":"2.0","id":1,"error":{"message":"bad handshake"}}"#;
        assert_eq!(response_for(err, 1).unwrap().unwrap_err(), "bad handshake");
    }
}
