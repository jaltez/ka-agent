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

/// Shortest recurrence the model may schedule: every fire is a full agent
/// turn, so a runaway `every 1s` would burn the budget in minutes.
const MIN_MODEL_CRON_EVERY_MS: u64 = 60_000;
/// Live crons the model may hold at once (operator crons don't count).
const MAX_MODEL_CRONS: usize = 50;

/// Tool definition: name, description, JSON schema, effect class.
struct ToolDef {
    name: String,
    description: String,
    schema: Value,
}

/// One loadable plugin: its validated manifest and installed root. Loaded
/// once per bridge start; a digest mismatch or invalid manifest disables the
/// plugin with a visible reason (never half-loaded, SEED.md §6.5).
pub struct LoadedPlugin {
    pub manifest: ka_agent_plugins::Manifest,
    pub root: PathBuf,
}

/// Load every enabled, verified plugin under `<data>/plugins`. Returns the
/// plugins plus (id, reason) for each refused one, for `tools/list` stderr
/// and `plugin list`.
pub fn load_plugins() -> (Vec<LoadedPlugin>, Vec<(String, String)>) {
    let mut ok = Vec::new();
    let mut refused = Vec::new();
    let Some(root) = data_root().ok() else {
        return (ok, refused);
    };
    let plugins_root = root.join("plugins");
    let lock = match ka_agent_plugins::Lockfile::load(&plugins_root.join("installed.json")) {
        Ok(l) => l,
        Err(e) => {
            refused.push(("<lockfile>".into(), e.to_string()));
            return (ok, refused);
        }
    };
    for record in &lock.plugins {
        if !record.enabled {
            continue;
        }
        let dir = plugins_root.join(&record.id);
        let mut refuse = |reason: String| refused.push((record.id.clone(), reason));
        match ka_agent_plugins::Manifest::load(&dir.join(ka_agent_plugins::MANIFEST_NAME)) {
            Err(e) => refuse(format!("manifest: {e}")),
            Ok(manifest) => match ka_agent_plugins::tree_digest(&dir) {
                Err(e) => refuse(format!("digest: {e}")),
                Ok(digest) => {
                    if digest != record.digest {
                        refuse(
                            "installed files no longer match the pinned digest \
                                   — reinstall the plugin or run `ka-agent plugin rm`"
                                .to_string(),
                        );
                    } else {
                        ok.push(LoadedPlugin {
                            manifest,
                            root: dir,
                        });
                    }
                }
            },
        }
    }
    (ok, refused)
}

/// Run one plugin tool call: scope grants → intent (write effects) → the
/// bubblewrap sandbox → the plugin's answer.
fn call_plugin(
    plugin: &LoadedPlugin,
    tool: &ka_agent_plugins::manifest::ToolSection,
    args: &Value,
    grants: &grants::Grants,
) -> Result<String, String> {
    for scope in &tool.scopes {
        if !grants.check(scope) {
            return Err(format!(
                "no grant for {scope} — ask the operator to run: ka-agent grant add {scope}"
            ));
        }
    }
    // Declared secrets only (SEED.md §6.4): resolved from the instance store,
    // never inherited from this process.
    let secrets_dir = data_root().map_err(|e| e.to_string())?.join("secrets");
    let mut secrets = std::collections::HashMap::new();
    for name in &plugin.manifest.requires.secrets {
        let value = ka_agent_core::secret::resolve(name, &secrets_dir)
            .map_err(|e| format!("secret {name:?}: {e}"))?;
        secrets.insert(name.clone(), value);
    }
    // Write effects are intents, exactly like first-party writes: logged
    // before the call, settled after.
    let mut intent: Option<(String, String)> = None; // (id, summary)
    if tool.effect != "read" {
        let root = data_root().map_err(|e| e.to_string())?;
        let store = ka_agent_store::Store::open(&root.join("state.db"))
            .map_err(|e| format!("store: {e}"))?;
        let now = ka_agent_core::now_ms();
        let id = ka_agent_core::IntentId::generate();
        let row = ka_agent_store::IntentRow {
            id: id.to_string(),
            tool: tool.name.clone(),
            args_hash: format!("{:x}", sha256_of(args)),
            status: "executing".to_string(),
            external_ref: None,
            summary: Some(format!(
                "plugin {} · {}",
                plugin.manifest.plugin.id, tool.name
            )),
            created_ms: now,
            updated_ms: now,
        };
        store.insert_intent(&row).ok();
        intent = Some((id.to_string(), tool.name.clone()));
    }
    let sandbox = ka_agent_plugins::Sandbox {
        plugin_root: plugin.root.clone(),
        command: plugin.manifest.plugin.entry.command.clone(),
        args: plugin.manifest.plugin.entry.args.clone(),
        secrets,
    };
    let answer = sandbox.call(&tool.name, args).map_err(|e| {
        settle_intent(intent.as_ref().map(|(id, _)| id.as_str()), "failed");
        format!("plugin {}: {e}", plugin.manifest.plugin.id)
    })?;
    if let Some((id, _)) = intent.as_ref() {
        settle_intent(Some(id), "done");
    }
    Ok(answer)
}

fn settle_intent(id: Option<&str>, status: &str) {
    let Some(id) = id else { return };
    let Ok(root) = data_root() else { return };
    let Ok(store) = ka_agent_store::Store::open(&root.join("state.db")) else {
        return;
    };
    store.update_intent_status(id, status, None).ok();
}

/// Stable short digest of the call arguments (the intent's idempotency key
/// material).
fn sha256_of(args: &Value) -> u64 {
    use std::hash::{Hash, Hasher};
    let text = serde_json::to_string(args).unwrap_or_default();
    let mut h = std::collections::hash_map::DefaultHasher::new();
    text.hash(&mut h);
    h.finish()
}

/// First-party tools plus every enabled plugin's declared tools. Plugin
/// tools carry a generic v1 schema (the manifest declares names, effects
/// and scopes; per-argument schemas arrive with plugin API v2).
fn tools(plugins: &[LoadedPlugin]) -> Vec<ToolDef> {
    let mut out = vec![
        ToolDef {
            name: "memory_propose".into(),
            description: "Propose a durable memory about the operator for their review. Keep it atomic (one fact, <=300 chars), state it in third person, and never include secrets. The operator reviews and accepts/rejects proposals; nothing is stored until they do.".into(),
            schema: json!({
                "type": "object",
                "properties": {
                    "text": {"type": "string", "description": "The memory fact, third person, atomic"}
                },
                "required": ["text"]
            }),
        },
        ToolDef {
            name: "notes_search".into(),
            description: "Search the operator's local markdown notes under the notes/ directory. Returns matching lines with file paths.".into(),
            schema: json!({
                "type": "object",
                "properties": {
                    "query": {"type": "string", "description": "Search text (case-insensitive substring match)"}
                },
                "required": ["query"]
            }),
        },
        ToolDef {
            name: "notes_append".into(),
            description: "Append a line to a note file in the notes/ directory. Creates the file if it does not exist. File names: [a-z0-9-] only.".into(),
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
            name: "notes_read".into(),
            description: "Read a note file from the notes/ directory.".into(),
            schema: json!({
                "type": "object",
                "properties": {
                    "file": {"type": "string", "description": "Note file name without extension"}
                },
                "required": ["file"]
            }),
        },
        ToolDef {
            name: "http_get".into(),
            description: "Fetch a URL (GET) from allowlisted hosts. The URL host must match a granted scope; private, loopback and link-local addresses are refused.".into(),
            schema: json!({
                "type": "object",
                "properties": {
                    "url": {"type": "string", "description": "The URL to fetch"}
                },
                "required": ["url"]
            }),
        },
        ToolDef {
            name: "github_issues_list".into(),
            description: "List open issues of a GitHub repository (read).".into(),
            schema: json!({
                "type": "object",
                "properties": {
                    "repo": {"type": "string", "description": "owner/name"}
                },
                "required": ["repo"]
            }),
        },
        ToolDef {
            name: "github_pulls_get".into(),
            description: "Read one pull request: number, title, state, author, body.".into(),
            schema: json!({
                "type": "object",
                "properties": {
                    "repo": {"type": "string", "description": "owner/name"},
                    "number": {"type": "integer"}
                },
                "required": ["repo", "number"]
            }),
        },
        ToolDef {
            name: "github_issue_comment".into(),
            description: "Comment on a GitHub issue (external write: logged as an intent, needs the github:issues:write grant).".into(),
            schema: json!({
                "type": "object",
                "properties": {
                    "repo": {"type": "string", "description": "owner/name"},
                    "number": {"type": "integer"},
                    "body": {"type": "string"}
                },
                "required": ["repo", "number", "body"]
            }),
        },
        ToolDef {
            name: "github_pull_review".into(),
            description: "Submit a pull request review (external write: needs github:pulls:write).".into(),
            schema: json!({
                "type": "object",
                "properties": {
                    "repo": {"type": "string", "description": "owner/name"},
                    "number": {"type": "integer"},
                    "body": {"type": "string"},
                    "event": {"type": "string", "enum": ["APPROVE", "REQUEST_CHANGES", "COMMENT"]}
                },
                "required": ["repo", "number", "body", "event"]
            }),
        },
        ToolDef {
            name: "cron_manage".into(),
            description: "Manage ka-agent cron jobs: schedule prompts to run later (one-shot 'in' or 'at') or on a fixed interval ('every'), and list/pause/resume/remove/trigger existing jobs. A scheduled prompt runs as a normal operator turn through the approval flow. Use 'at' with the operator's UTC offset for clock times.".into(),
            schema: json!({
                "type": "object",
                "properties": {
                    "action": {"type": "string", "enum": ["schedule", "list", "pause", "resume", "remove", "trigger"]},
                    "text": {"type": "string", "description": "schedule: the prompt to run"},
                    "in": {"type": "string", "description": "schedule: one-shot delay, Ns/Nm/Nh/Nd"},
                    "at": {"type": "string", "description": "schedule: one-shot absolute time, RFC 3339 with offset (e.g. 2026-10-20T09:00:00+02:00)"},
                    "every": {"type": "string", "description": "schedule: recurring interval, Nm/Nh/Nd (minimum 60s)"},
                    "id": {"type": "string", "description": "pause/resume/remove/trigger: job id or id prefix"}
                },
                "required": ["action"]
            }),
        },
    ];
    for plugin in plugins {
        for tool in &plugin.manifest.tools {
            out.push(ToolDef {
                name: tool.name.clone(),
                description: format!(
                    "[plugin {} v{}] {}",
                    plugin.manifest.plugin.id,
                    plugin.manifest.plugin.version,
                    plugin.manifest.plugin.description
                ),
                schema: json!({"type": "object"}),
            });
        }
    }
    out
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

fn handle_tool_call(
    name: &str,
    args: &Value,
    grants: &grants::Grants,
    plugins: &[LoadedPlugin],
) -> Result<String, String> {
    // Plugin tools are namespaced `<id>.<tool>`; first-party names have no
    // dot in their first segment, so the check is exact, never a prefix.
    for plugin in plugins {
        if let Some(tool) = plugin.manifest.tool(name) {
            return call_plugin(plugin, tool, args, grants);
        }
    }
    match name {
        "memory_propose" => {
            let text = args["text"].as_str().ok_or("missing 'text'")?;
            if text.is_empty() || text.len() > 300 {
                return Err("text must be 1-300 chars".into());
            }
            ka_agent_memory::screen_proposal(text)?;
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
                    if path.extension().is_some_and(|e| e == "md")
                        && let Ok(content) = std::fs::read_to_string(&path)
                    {
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
                        Ok(format!(
                            "{}… (truncated)",
                            ka_agent_core::truncate_at_char(&content, 8000)
                        ))
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
            // SSRF: refuse private/loopback/link-local targets unless
            // explicitly granted; every redirect hop re-checks the same rule.
            if !grants.check("http:get:private") && http_target_is_private(&host) {
                return Err(
                    "private/loopback/link-local hosts require the http:get:private grant".into(),
                );
            }
            let body = http_get_no_redirect(&parsed, 15, grants)?;
            if body.len() > 200_000 {
                Ok(format!(
                    "{}… (truncated at 200 KB)",
                    ka_agent_core::truncate_at_char(&body, 200_000)
                ))
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
            let token = github_token()?;
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
            let token = github_token()?;
            let body = github_api(&token, &format!("/repos/{repo}/pulls/{number}"))?;
            let title = body["title"].as_str().unwrap_or("?");
            let state = body["state"].as_str().unwrap_or("?");
            let user = body["user"]["login"].as_str().unwrap_or("?");
            let body_text = body["body"].as_str().unwrap_or("");
            Ok(format!(
                "#{number} [{state}] {title}\nby {user}\n{body_text}"
            ))
        }
        "github_issue_comment" => {
            let repo = args["repo"].as_str().ok_or("missing 'repo'")?;
            let number = args["number"].as_u64().ok_or("missing 'number'")?;
            let body_text = args["body"].as_str().ok_or("missing 'body'")?;
            let scope = "github:issues:write";
            if !grants.check(scope) {
                return Err(format!(
                    "no grant for {scope} — ask the operator to run: ka-agent grant add {scope}"
                ));
            }
            let token = github_token()?;
            let root = data_root()?;
            let store = ka_agent_store::Store::open(&root.join("state.db"))
                .map_err(|e| format!("store: {e}"))?;
            let hash = call_hash("github_issue_comment", args);
            ensure_not_duplicate(&store, "github_issue_comment", &hash)?;
            let now = ka_agent_core::now_ms();
            let intent_id = ka_agent_core::IntentId::generate();
            let intent = ka_agent_store::IntentRow {
                id: intent_id.to_string(),
                tool: "github_issue_comment".to_string(),
                args_hash: hash,
                status: "executing".to_string(),
                external_ref: None,
                summary: Some(format!("comment on #{number} in {repo}")),
                created_ms: now,
                updated_ms: now,
            };
            store.insert_intent(&intent).ok(); // intent log is best-effort
            let body = github_api_post(
                &token,
                &format!("/repos/{repo}/issues/{number}/comments"),
                &json!({"body": body_text}),
            )?;
            let html_url = body["html_url"].as_str().unwrap_or("?");
            store
                .update_intent_status(&intent_id.to_string(), "done", Some(html_url))
                .ok();
            Ok(format!("comment posted: {html_url}"))
        }
        "github_pull_review" => {
            let repo = args["repo"].as_str().ok_or("missing 'repo'")?;
            let number = args["number"].as_u64().ok_or("missing 'number'")?;
            let body_text = args["body"].as_str().ok_or("missing 'body'")?;
            let event = args["event"].as_str().unwrap_or("COMMENT");
            let scope = "github:pulls:write";
            if !grants.check(scope) {
                return Err(format!(
                    "no grant for {scope} — ask the operator to run: ka-agent grant add {scope}"
                ));
            }
            let token = github_token()?;
            let root = data_root()?;
            let store = ka_agent_store::Store::open(&root.join("state.db"))
                .map_err(|e| format!("store: {e}"))?;
            let hash = call_hash("github_pull_review", args);
            ensure_not_duplicate(&store, "github_pull_review", &hash)?;
            let now = ka_agent_core::now_ms();
            let intent_id = ka_agent_core::IntentId::generate();
            let intent = ka_agent_store::IntentRow {
                id: intent_id.to_string(),
                tool: "github_pull_review".to_string(),
                args_hash: hash,
                status: "executing".to_string(),
                external_ref: None,
                summary: Some(format!("review on PR #{number} in {repo}")),
                created_ms: now,
                updated_ms: now,
            };
            store.insert_intent(&intent).ok(); // intent log is best-effort
            let body = github_api_post(
                &token,
                &format!("/repos/{repo}/pulls/{number}/reviews"),
                &json!({"body": body_text, "event": event}),
            )?;
            let html_url = body["html_url"].as_str().unwrap_or("?");
            store
                .update_intent_status(&intent_id.to_string(), "done", Some(html_url))
                .ok();
            Ok(format!("review posted: {html_url}"))
        }
        "cron_manage" => {
            // The bridge is a separate process: it operates on the store
            // directly (same pattern as memory_propose). It only ever
            // touches `kind = 'cron'` rows, never reminders.
            let action = args["action"].as_str().ok_or("missing 'action'")?;
            let root = data_root()?;
            let store = ka_agent_store::Store::open(&root.join("state.db"))
                .map_err(|e| format!("store: {e}"))?;
            match action {
                "schedule" => {
                    let text = args["text"].as_str().ok_or("missing 'text'")?;
                    if text.trim().is_empty() {
                        return Err("text must not be empty".into());
                    }
                    let every = args["every"].as_str();
                    let once = args["in"].as_str();
                    let at = args["at"].as_str();
                    let (recurrence_ms, first_at_ms) = match (every, once, at) {
                        (Some(d), None, None) => {
                            let ms = ka_agent_core::parse_duration(d).ok_or_else(|| {
                                format!("bad duration {d:?}; use Ns, Nm, Nh or Nd")
                            })?;
                            if ms < MIN_MODEL_CRON_EVERY_MS {
                                return Err(format!(
                                    "'every' must be at least {}s — each run is a full agent turn",
                                    MIN_MODEL_CRON_EVERY_MS / 1000
                                ));
                            }
                            (Some(ms as i64), ka_agent_core::now_ms() + ms as i64)
                        }
                        (None, Some(d), None) => {
                            let ms = ka_agent_core::parse_duration(d).ok_or_else(|| {
                                format!("bad duration {d:?}; use Ns, Nm, Nh or Nd")
                            })?;
                            (None, ka_agent_core::now_ms() + ms as i64)
                        }
                        (None, None, Some(rfc3339)) => {
                            let ts: jiff::Timestamp = rfc3339.parse()
                                .map_err(|e| format!("bad 'at' {rfc3339:?}: {e} (use RFC 3339 with offset, e.g. 2026-10-20T09:00:00+02:00)"))?;
                            if ts.as_millisecond() <= ka_agent_core::now_ms() {
                                return Err("'at' is in the past".into());
                            }
                            (None, ts.as_millisecond())
                        }
                        _ => {
                            return Err(
                                "give exactly one of 'in' (one-shot), 'every' (recurring) or 'at' (absolute RFC 3339)".into(),
                            );
                        }
                    };
                    let live = store
                        .cron_tasks()
                        .map_err(|e| format!("store: {e}"))?
                        .iter()
                        .filter(|t| {
                            t.payload
                                .as_deref()
                                .and_then(ka_agent_core::CronSpec::from_payload)
                                .is_some_and(|c| c.source == "kagent")
                        })
                        .count();
                    if live >= MAX_MODEL_CRONS {
                        return Err(format!(
                            "{live} model-scheduled crons are live (limit {MAX_MODEL_CRONS}) — remove some first"
                        ));
                    }
                    let spec = ka_agent_core::CronSpec {
                        text: text.to_string(),
                        source: "kagent".into(),
                        recurrence_ms,
                        mode: ka_agent_core::CronMode::Agent,
                        at_local: None,
                        weekday: None,
                    };
                    let mut task = ka_agent_core::Task::new(spec.text.clone(), "cron", false);
                    task.scheduled_at_ms = Some(first_at_ms);
                    task.payload = Some(spec.to_payload());
                    store
                        .insert_task(&task)
                        .map_err(|e| format!("insert: {e}"))?;
                    Ok(format!(
                        "cron {} scheduled ({})",
                        ka_agent_core::ids::short_id(task.id.as_str()),
                        match recurrence_ms {
                            Some(ms) => format!("every {}", crate::commands::fmt_interval(ms)),
                            None => "once".to_string(),
                        }
                    ))
                }
                "list" => {
                    let crons = store.cron_tasks().map_err(|e| format!("store: {e}"))?;
                    Ok(if crons.is_empty() {
                        "no crons".to_string()
                    } else {
                        let now = jiff::Timestamp::now().to_zoned(jiff::tz::TimeZone::system());
                        crate::commands::cron_list_text(&crons, &now)
                    })
                }
                "pause" | "resume" | "remove" | "trigger" => {
                    let id = args["id"].as_str().ok_or("missing 'id'")?;
                    if id.trim().is_empty() {
                        return Err("missing 'id'".into());
                    }
                    let crons = store.cron_tasks().map_err(|e| format!("store: {e}"))?;
                    let mut hits = crons
                        .iter()
                        .filter(|t| ka_agent_core::ids::id_matches(t.id.as_str(), id));
                    let task = hits
                        .next()
                        .ok_or_else(|| format!("no live cron id starts with {id:?}"))?;
                    if hits.next().is_some() {
                        return Err(format!(
                            "id prefix {id:?} matches several crons — use more characters (see action \"list\")"
                        ));
                    }
                    let short = ka_agent_core::ids::short_id(task.id.as_str());
                    match action {
                        "pause" => {
                            store
                                .set_task_status(&task.id, "interrupted")
                                .map_err(|e| format!("pause: {e}"))?;
                            Ok(format!("cron {short} paused"))
                        }
                        "remove" => {
                            store
                                .set_task_status(&task.id, "cancelled")
                                .map_err(|e| format!("remove: {e}"))?;
                            Ok(format!("cron {short} removed"))
                        }
                        "trigger" => {
                            store
                                .update_task_schedule(
                                    &task.id,
                                    Some(ka_agent_core::now_ms()),
                                    "queued",
                                )
                                .map_err(|e| format!("trigger: {e}"))?;
                            Ok(format!(
                                "cron {short} armed; fires on the next scheduler tick"
                            ))
                        }
                        _ => {
                            // Resume: skip missed occupations for recurring
                            // jobs; a one-shot fires now.
                            let now = ka_agent_core::now_ms();
                            let spec = task
                                .payload
                                .as_deref()
                                .and_then(ka_agent_core::CronSpec::from_payload);
                            let next = match (
                                spec.as_ref().and_then(|s| s.recurrence_ms),
                                task.scheduled_at_ms,
                            ) {
                                (Some(rec), Some(sched)) if rec > 0 => {
                                    let mut n = sched;
                                    while n <= now {
                                        n += rec;
                                    }
                                    n
                                }
                                _ => now,
                            };
                            store
                                .update_task_schedule(&task.id, Some(next), "queued")
                                .map_err(|e| format!("resume: {e}"))?;
                            Ok(format!("cron {short} resumed"))
                        }
                    }
                }
                other => Err(format!(
                    "unknown action {other:?}; use schedule|list|pause|resume|remove|trigger"
                )),
            }
        }
        _ => Err(format!("unknown tool: {name}")),
    }
}

/// Content hash of one tool call: sha256 over the tool name and the
/// serialized arguments. The intent log's idempotency key (SEED.md §7.3).
fn call_hash(tool: &str, args: &Value) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(tool.as_bytes());
    h.update(b"\0");
    h.update(args.to_string().as_bytes());
    let d = h.finalize();
    d.iter().map(|b| format!("{b:02x}")).collect()
}

/// Refuse a duplicate external write: an executing or succeeded intent
/// with the same tool + args hash means the effect already happened (or is
/// in flight). Fail closed instead of writing twice; failed attempts stay
/// retryable (`active_intent_by_hash` skips them).
fn ensure_not_duplicate(
    store: &ka_agent_store::Store,
    tool: &str,
    hash: &str,
) -> Result<(), String> {
    match store.active_intent_by_hash(tool, hash) {
        Ok(Some(prev)) => Err(format!(
            "duplicate external write refused: {tool} with identical arguments is already \
             {status} (intent {id}) — change the arguments if you meant a new effect",
            status = prev.status,
            id = prev.id
        )),
        Ok(None) => Ok(()),
        Err(e) => Err(format!("intent log: {e}")),
    }
}

/// Resolve the GitHub token: env first (the worker injects it), then the
/// secret-store file tier (`<data>/secrets/github.token`). The worker's env
/// sanitizer strips credential-shaped variables, so the file tier is the
/// durable path (F13).
fn github_token() -> Result<String, String> {
    if let Ok(t) = std::env::var("KA_AGENT_GITHUB_TOKEN")
        && !t.trim().is_empty()
    {
        return Ok(t);
    }
    let root = data_root().map_err(|e| e.to_string())?;
    let path = root.join("secrets/github.token");
    std::fs::read_to_string(&path)
        .map(|t| t.trim().to_string())
        .ok()
        .filter(|t| !t.is_empty())
        .ok_or_else(|| {
            "no GitHub token: set KA_AGENT_GITHUB_TOKEN or place one at <data>/secrets/github.token"
                .to_string()
        })
}

/// SSRF guard for `http_get`: refuse loopback, link-local (cloud metadata),
/// unique-local, private and unspecified targets — as IP literals in any
/// form (v4/v6, bracketed) — unless the operator granted `http:get:private`.
/// Redirects are followed manually and every hop re-checks the same guard.
pub(crate) fn http_target_is_private(host: &str) -> bool {
    let bare = host.trim_start_matches('[').trim_end_matches(']');
    if let Ok(ip) = bare.parse::<std::net::IpAddr>() {
        return match ip {
            std::net::IpAddr::V4(v4) => {
                v4.is_loopback()
                    || v4.is_link_local()
                    || v4.is_private()
                    || v4.is_unspecified()
                    || v4.is_broadcast()
                    // 172.16/12 is `is_private`, but 169.254/16 (metadata)
                    // and 0.0.0.0/8 are covered above.
                    || v4.octets()[0] == 100 && (v4.octets()[1] & 0xC0) == 64 // 100.64/10 CGNAT
            }
            std::net::IpAddr::V6(v6) => {
                v6.is_loopback()
                    || v6.is_unspecified()
                    || (v6.segments()[0] & 0xfe00) == 0xfc00 // unique-local fc00::/7
                    || (v6.segments()[0] & 0xffc0) == 0xfe80 // link-local fe80::/10
            }
        };
    }
    // Names: only the canonical localhost forms; a resolving name is checked
    // after resolution by the caller when possible.
    bare.eq_ignore_ascii_case("localhost")
        || bare.eq_ignore_ascii_case("localhost.localdomain")
        || bare.ends_with(".localhost")
        || bare.ends_with(".local")
        || bare.ends_with(".internal")
}

/// A blocking HTTP client for tool calls: per-call (they are rare), no
/// redirects (the SSRF guard walks each hop itself), 15 s ceiling, and the
/// product User-Agent. Runs in the `__mcp` bridge process or on
/// `spawn_blocking` threads — never inside the async reactor.
fn http_client(timeout_secs: u64) -> Result<reqwest::blocking::Client, String> {
    reqwest::blocking::Client::builder()
        .user_agent("ka-agent")
        .timeout(std::time::Duration::from_secs(timeout_secs))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|e| format!("http client: {e}"))
}

/// GitHub API POST helper (write operations).
fn github_api_post(token: &str, path: &str, body: &Value) -> Result<Value, String> {
    let client = http_client(15)?;
    let url = format!("https://api.github.com{path}");
    let response = client
        .post(&url)
        .header(reqwest::header::AUTHORIZATION, format!("Bearer {token}"))
        .header(reqwest::header::ACCEPT, "application/vnd.github+json")
        .json(body)
        .send()
        .map_err(|e| format!("github: {e}"))?;
    if !response.status().is_success() {
        return Err(format!("github: status {}", response.status()));
    }
    let text = response.text().map_err(|e| format!("github body: {e}"))?;
    let result: Value = serde_json::from_str(&text).map_err(|e| format!("github json: {e}"))?;
    Ok(result)
}

/// Blocking HTTP GET, returns the response body as text. The `http_get`
/// tool with manual redirects: each hop's host must pass the same SSRF
/// guard as the first (a public host redirecting to 169.254.169.254 must
/// not become a metadata read).
pub(crate) fn http_get_no_redirect(
    url: &url::Url,
    timeout_secs: u64,
    grants: &grants::Grants,
) -> Result<String, String> {
    let allow_private = grants.check("http:get:private");
    let client = http_client(timeout_secs)?;
    let mut current = url.clone();
    for _ in 0..5 {
        let resp = client
            .get(current.as_str())
            .send()
            .map_err(|e| format!("http: {e}"))?;
        let status = resp.status().as_u16();
        if (301..=308).contains(&status) {
            let location = resp
                .headers()
                .get(reqwest::header::LOCATION)
                .and_then(|v| v.to_str().ok())
                .unwrap_or("")
                .to_string();
            let next = url::Url::parse(&location)
                .or_else(|_| current.join(&location))
                .map_err(|e| format!("redirect target: {e}"))?;
            if !allow_private && http_target_is_private(next.host_str().unwrap_or("")) {
                return Err(format!(
                    "redirect to private target {} refused (grant http:get:private to allow)",
                    next.host_str().unwrap_or("?")
                ));
            }
            current = next;
            continue;
        }
        if !resp.status().is_success() {
            return Err(format!("http status {}", resp.status()));
        }
        let content_type = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        if !content_type.starts_with("text/")
            && !content_type.starts_with("application/json")
            && !content_type.starts_with("application/xml")
        {
            return Err(format!("unsupported content-type: {content_type}"));
        }
        return resp.text().map_err(|e| format!("read body: {e}"));
    }
    Err("too many redirects".into())
}

/// Simple GitHub API GET.
fn github_api(token: &str, path: &str) -> Result<Value, String> {
    let client = http_client(15)?;
    let url = format!("https://api.github.com{path}");
    let response = client
        .get(&url)
        .header(reqwest::header::AUTHORIZATION, format!("Bearer {token}"))
        .header(reqwest::header::ACCEPT, "application/vnd.github+json")
        .send()
        .map_err(|e| format!("github: {e}"))?;
    if !response.status().is_success() {
        return Err(format!("github: status {}", response.status()));
    }
    let text = response.text().map_err(|e| format!("github body: {e}"))?;
    let body: Value = serde_json::from_str(&text).map_err(|e| format!("github json: {e}"))?;
    Ok(body)
}

/// Entry point for `ka-agent __mcp`. Reads JSON-RPC from stdin, writes to
/// stdout, logs to stderr.
pub fn run_mcp() -> Result<(), String> {
    let (plugins, refused) = load_plugins();
    for (id, reason) in &refused {
        eprintln!("kagent: plugin {id} disabled: {reason}");
    }
    eprintln!(
        "kagent: {} plugin(s) loaded, {} refused, data={:?}",
        plugins.len(),
        refused.len(),
        std::env::var_os("KA_AGENT_DATA_DIR")
    );
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
                "tools": tools(&plugins).iter().map(|t| json!({
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
                    Ok(grants) => match handle_tool_call(tool, args, &grants, &plugins) {
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

#[cfg(test)]
mod ssrf_tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::http_target_is_private;

    #[test]
    fn private_and_metadata_targets_are_refused() {
        // Loopback in full, not just 127.0.0.1.
        for host in [
            "127.0.0.1",
            "127.8.8.8",
            "0.0.0.0",
            "10.1.2.3",
            "192.168.0.1",
            "172.16.0.9",
            "169.254.169.254",
            "100.64.0.1",
            "255.255.255.255",
            "localhost",
            "LOCALHOST",
            "foo.localhost",
            "printer.local",
            "db.internal",
        ] {
            assert!(http_target_is_private(host), "{host} must be private");
        }
        // IPv6: loopback, unspecified, unique-local, link-local (bracketed or bare).
        for host in [
            "::1",
            "[::1]",
            "0:0:0:0:0:0:0:0",
            "fd00::1",
            "[fd12::9]",
            "fe80::1",
            "[fe80::1]",
        ] {
            assert!(http_target_is_private(host), "{host} must be private");
        }
        // Public targets stay reachable.
        for host in [
            "example.com",
            "api.github.com",
            "93.184.216.34",
            "2606:2800:220:1:248:1893:25c8:1946",
        ] {
            assert!(!http_target_is_private(host), "{host} must not be private");
        }
    }
}
