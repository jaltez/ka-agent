//! GitHub tools: reads are plain API calls, writes are intents (logged
//! before, settled after) with a duplicate guard. Split from the old MCP
//! bridge; the transport is the daemon's (no re-injected token env).

use serde_json::{Value, json};

use super::{ToolCtx, ToolFuture};

fn github_token(ctx: &ToolCtx) -> Result<String, String> {
    let path = ctx.paths.root().join("secrets/github.token");
    std::fs::read_to_string(&path)
        .map(|t| t.trim().to_string())
        .ok()
        .filter(|t| !t.is_empty())
        .ok_or_else(|| {
            "no GitHub token: place one at <data>/secrets/github.token (ka-agent secret set github.token)"
                .to_string()
        })
}

pub(crate) fn issues_list_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "repo": {"type": "string", "description": "owner/name"},
            "state": {"type": "string", "description": "open | closed | all (default open)"},
            "limit": {"type": "integer", "description": "max issues, 1-30 (default 10)"}
        },
        "required": ["repo"]
    })
}

pub(crate) fn issues_list<'a>(ctx: &'a ToolCtx, args: Value) -> ToolFuture<'a> {
    Box::pin(async move {
        let repo = args["repo"].as_str().ok_or("missing 'repo'")?.to_string();
        let state = args["state"].as_str().unwrap_or("open").to_string();
        let limit = args["limit"].as_u64().unwrap_or(10).min(30);
        let token = github_token(ctx)?;
        let path = format!("/repos/{repo}/issues?state={state}&per_page={limit}");
        let body = tokio::task::spawn_blocking(move || github_api(&token, &path))
            .await
            .map_err(|e| format!("github task: {e}"))??;
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
    })
}

pub(crate) fn pulls_get_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "repo": {"type": "string", "description": "owner/name"},
            "number": {"type": "integer", "description": "pull request number"}
        },
        "required": ["repo", "number"]
    })
}

pub(crate) fn pulls_get<'a>(ctx: &'a ToolCtx, args: Value) -> ToolFuture<'a> {
    Box::pin(async move {
        let repo = args["repo"].as_str().ok_or("missing 'repo'")?.to_string();
        let number = args["number"].as_u64().ok_or("missing 'number'")?;
        let token = github_token(ctx)?;
        let path = format!("/repos/{repo}/pulls/{number}");
        let body = tokio::task::spawn_blocking(move || github_api(&token, &path))
            .await
            .map_err(|e| format!("github task: {e}"))??;
        let title = body["title"].as_str().unwrap_or("?");
        let state = body["state"].as_str().unwrap_or("?");
        let user = body["user"]["login"].as_str().unwrap_or("?");
        let body_text = body["body"].as_str().unwrap_or("");
        Ok(format!(
            "#{number} [{state}] {title}\nby {user}\n{body_text}"
        ))
    })
}

pub(crate) fn issue_comment_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "repo": {"type": "string", "description": "owner/name"},
            "number": {"type": "integer", "description": "issue number"},
            "body": {"type": "string", "description": "comment body (markdown)"}
        },
        "required": ["repo", "number", "body"]
    })
}

pub(crate) fn issue_comment<'a>(ctx: &'a ToolCtx, args: Value) -> ToolFuture<'a> {
    Box::pin(async move {
        let repo = args["repo"].as_str().ok_or("missing 'repo'")?.to_string();
        let number = args["number"].as_u64().ok_or("missing 'number'")?;
        let body_text = args["body"].as_str().ok_or("missing 'body'")?.to_string();
        let scope = "github:issues:write";
        let grants = crate::grants::Grants::load(&ctx.store).map_err(|e| format!("grants: {e}"))?;
        if !grants.check(scope) {
            return Err(format!(
                "no grant for {scope} — ask the operator to run: ka-agent grant add {scope}"
            ));
        }
        let token = github_token(ctx)?;
        let hash = call_hash("github_issue_comment", &args);
        ensure_not_duplicate(&ctx.store, "github_issue_comment", &hash)?;
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
        ctx.store.insert_intent(&intent).ok();
        let path = format!("/repos/{repo}/issues/{number}/comments");
        let payload = json!({"body": body_text});
        let body = tokio::task::spawn_blocking(move || github_api_post(&token, &path, &payload))
            .await
            .map_err(|e| format!("github task: {e}"))?;
        let body = match body {
            Ok(b) => b,
            Err(e) => {
                ctx.store
                    .update_intent_status(&intent_id.to_string(), "failed", None)
                    .ok();
                return Err(e);
            }
        };
        let html_url = body["html_url"].as_str().unwrap_or("?");
        ctx.store
            .update_intent_status(&intent_id.to_string(), "done", Some(html_url))
            .ok();
        Ok(format!("comment posted: {html_url}"))
    })
}

pub(crate) fn pull_review_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "repo": {"type": "string", "description": "owner/name"},
            "number": {"type": "integer", "description": "pull request number"},
            "body": {"type": "string", "description": "review body (markdown)"},
            "event": {"type": "string", "description": "COMMENT | APPROVE | REQUEST_CHANGES (default COMMENT)"}
        },
        "required": ["repo", "number", "body"]
    })
}

pub(crate) fn pull_review<'a>(ctx: &'a ToolCtx, args: Value) -> ToolFuture<'a> {
    Box::pin(async move {
        let repo = args["repo"].as_str().ok_or("missing 'repo'")?.to_string();
        let number = args["number"].as_u64().ok_or("missing 'number'")?;
        let body_text = args["body"].as_str().ok_or("missing 'body'")?.to_string();
        let event = args["event"].as_str().unwrap_or("COMMENT").to_string();
        let scope = "github:pulls:write";
        let grants = crate::grants::Grants::load(&ctx.store).map_err(|e| format!("grants: {e}"))?;
        if !grants.check(scope) {
            return Err(format!(
                "no grant for {scope} — ask the operator to run: ka-agent grant add {scope}"
            ));
        }
        let token = github_token(ctx)?;
        let hash = call_hash("github_pull_review", &args);
        ensure_not_duplicate(&ctx.store, "github_pull_review", &hash)?;
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
        ctx.store.insert_intent(&intent).ok();
        let path = format!("/repos/{repo}/pulls/{number}/reviews");
        let payload = json!({"body": body_text, "event": event});
        let body = tokio::task::spawn_blocking(move || github_api_post(&token, &path, &payload))
            .await
            .map_err(|e| format!("github task: {e}"))?;
        let body = match body {
            Ok(b) => b,
            Err(e) => {
                ctx.store
                    .update_intent_status(&intent_id.to_string(), "failed", None)
                    .ok();
                return Err(e);
            }
        };
        let html_url = body["html_url"].as_str().unwrap_or("?");
        ctx.store
            .update_intent_status(&intent_id.to_string(), "done", Some(html_url))
            .ok();
        Ok(format!("review posted: {html_url}"))
    })
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

/// GitHub API POST helper (write operations).
fn github_api_post(token: &str, path: &str, body: &Value) -> Result<Value, String> {
    let client = super::http::http_client(15)?;
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

/// Simple GitHub API GET.
fn github_api(token: &str, path: &str) -> Result<Value, String> {
    let client = super::http::http_client(15)?;
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
