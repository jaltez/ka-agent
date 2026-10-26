//! The thin MCP relay (`ka-agent __mcp`, spawned by the engine as the
//! `kagent` server): a blocking stdio JSON-RPC loop that answers
//! `initialize` locally and relays `tools/list` / `tools/call` to the
//! daemon over its local IPC endpoint — one connection per request. The
//! tools themselves run in the daemon (one store reader, one place to
//! audit); a daemon that is down returns an `isError` result, never a
//! hung engine.

use std::io::{BufRead, Write};

use serde_json::{Value, json};

use crate::ipc;

/// First line ceiling on a relay response (tool results are capped well
/// below this by the handlers).
const MAX_REPLY: u64 = 4 * 1024 * 1024;

fn relay(paths: &ka_agent_runtime::paths::Paths, request: Value) -> Result<Value, String> {
    let stream = ipc::connect_blocking(paths).map_err(|_| "ka-agent is not running".to_string())?;
    stream
        .set_read_timeout(Some(std::time::Duration::from_secs(120)))
        .map_err(|e| format!("ipc timeout: {e}"))?;
    {
        let mut w = stream.try_clone().map_err(|e| format!("ipc: {e}"))?;
        w.write_all(format!("{request}\n").as_bytes())
            .map_err(|e| format!("ipc write: {e}"))?;
        w.flush().map_err(|e| format!("ipc flush: {e}"))?;
    }
    let mut reader = std::io::BufReader::new(stream);
    let mut line = String::new();
    let mut taken: u64 = 0;
    loop {
        let mut chunk = String::new();
        let n = reader
            .read_line(&mut chunk)
            .map_err(|e| format!("ipc read: {e}"))?;
        if n == 0 {
            break;
        }
        taken += n as u64;
        if taken > MAX_REPLY {
            return Err("relay reply too large".into());
        }
        line.push_str(&chunk);
        if line.ends_with('\n') {
            break;
        }
    }
    serde_json::from_str(line.trim()).map_err(|e| format!("relay reply: {e}"))
}

/// JSON-RPC result for one method, as the engine expects it.
fn handle(paths: &ka_agent_runtime::paths::Paths, msg: &Value) -> Value {
    let method = msg["method"].as_str().unwrap_or("");
    match method {
        "initialize" => json!({
            "protocolVersion": "2025-06-18",
            "capabilities": {"tools": {}},
            "serverInfo": {"name": "kagent", "version": env!("CARGO_PKG_VERSION")}
        }),
        "tools/list" => match relay(paths, json!({"op": "tool_list"})) {
            Ok(v) if v.get("ok").and_then(Value::as_bool) == Some(true) => {
                json!({"tools": v["tools"].clone()})
            }
            Ok(v) => json!({
                "content": [{"type": "text", "text": v["error"].as_str()
                    .unwrap_or("tool list failed")}],
                "isError": true
            }),
            Err(e) => json!({
                "content": [{"type": "text", "text": e}],
                "isError": true
            }),
        },
        "tools/call" => {
            let name = msg["params"]["name"].as_str().unwrap_or("");
            let args = msg["params"]["arguments"].clone();
            match relay(
                paths,
                json!({"op": "tool_call", "name": name, "args": args}),
            ) {
                Ok(v) if v.get("ok").and_then(Value::as_bool) == Some(true) => {
                    let text = v["text"].as_str().unwrap_or("").to_string();
                    let is_error = v["is_error"].as_bool().unwrap_or(false);
                    let mut out = json!({"content": [{"type": "text", "text": text}]});
                    if is_error {
                        out["isError"] = Value::Bool(true);
                    }
                    out
                }
                Ok(v) => json!({
                    "content": [{"type": "text", "text": v["error"].as_str()
                        .unwrap_or("tool call failed")}],
                    "isError": true
                }),
                Err(e) => json!({
                    "content": [{"type": "text", "text": e}],
                    "isError": true
                }),
            }
        }
        _ => json!({"error": {"code": -32601, "message": format!("method not found: {method}")}}),
    }
}

/// Entry point for `ka-agent __mcp`: blocking, one request at a time.
pub fn run() -> Result<(), String> {
    let paths = ka_agent_runtime::paths::Paths::new(ka_agent_runtime::paths::Paths::default_root());
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
        if msg["method"] == "notifications/initialized" {
            eprintln!("kagent: initialized");
            continue;
        }
        let id = msg.get("id").cloned();
        let result = handle(&paths, &msg);
        if let Some(id) = id {
            let response = json!({"jsonrpc": "2.0", "id": id, "result": result});
            writeln!(stdout, "{response}").map_err(|e| format!("stdout: {e}"))?;
            stdout.flush().map_err(|e| format!("stdout flush: {e}"))?;
        }
    }
    Ok(())
}
