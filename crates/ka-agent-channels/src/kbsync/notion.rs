//! Notion → knowledge base: search the integration's pages, render each
//! page's block children to markdown, index it. A page moved to the trash
//! disappears from search, and the sync prunes it.

use std::path::Path;

use ka_agent_store::{KbSourceRow, Store};
use serde_json::{Value, json};

use super::{Endpoints, MAX_DOCS, SyncReport, index_doc, token_of};

/// The API version this connector is written against (Notion versions are
/// pinned per request).
const NOTION_VERSION: &str = "2026-03-11";

pub async fn sync(
    store: &Store,
    src: &KbSourceRow,
    secrets: &Path,
    endpoints: &Endpoints,
) -> Result<SyncReport, String> {
    let token = token_of(src, secrets)?;
    let client = reqwest::Client::new();
    let mut pages: Vec<(String, String)> = Vec::new(); // (id, title)
    let mut cursor: Option<String> = None;
    loop {
        let mut body = json!({"filter": {"value": "page", "property": "object"}, "page_size": 100});
        if let Some(c) = &cursor {
            body["start_cursor"] = json!(c);
        }
        let resp = client
            .post(format!("{}/v1/search", endpoints.notion.trim_end_matches('/')))
            .bearer_auth(&token)
            .header("Notion-Version", NOTION_VERSION)
            .json(&body)
            .send()
            .await
            .map_err(|e| format!("notion search: {e}"))?;
        if !resp.status().is_success() {
            return Err(format!("notion search: HTTP {}", resp.status()));
        }
        let value: Value = resp.json().await.map_err(|e| format!("notion search: {e}"))?;
        for result in value["results"].as_array().cloned().unwrap_or_default() {
            let id = result["id"].as_str().unwrap_or("").to_string();
            if id.is_empty() {
                continue;
            }
            let title = result["properties"]
                .as_object()
                .and_then(|props| {
                    props.values().find_map(|p| {
                        p["title"]
                            .as_array()
                            .and_then(|t| t.first())
                            .and_then(|t| t["plain_text"].as_str())
                            .map(str::to_string)
                    })
                })
                .unwrap_or_else(|| id.clone());
            pages.push((id, title));
            if pages.len() >= MAX_DOCS {
                break;
            }
        }
        let next = value["next_cursor"].as_str().map(str::to_string);
        if value["has_more"].as_bool() != Some(true) || next.is_none() || pages.len() >= MAX_DOCS {
            break;
        }
        cursor = next;
    }
    let mut report = SyncReport {
        kb: src.kb_name.clone(),
        ..SyncReport::default()
    };
    let mut seen = Vec::new();
    for (id, title) in &pages {
        let body = render_page(&client, &token, id, 0, endpoints).await?;
        if body.len() > super::MAX_BYTES {
            continue;
        }
        let path = format!("{}.md", slug(title, id));
        match index_doc(store, &src.kb_name, &path, title, &body, "notion", ka_agent_core::now_ms())
        {
            Ok(true) => report.indexed += 1,
            Ok(false) => report.unchanged += 1,
            Err(_) => {}
        }
        seen.push(path);
    }
    report.removed = store
        .kb_prune_absent(&src.kb_name, &seen)
        .map_err(|e| e.to_string())?;
    Ok(report)
}

fn slug(title: &str, id: &str) -> String {
    let mut slug: String = title
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect();
    slug.truncate(48);
    let short: String = id.chars().filter(|c| c.is_ascii_alphanumeric()).rev().take(8).collect();
    let short: String = short.chars().rev().collect();
    format!("{}-{short}", slug.trim_matches('-'))
}

/// Walk a page's children into markdown (depth ≤ 4, capped).
async fn render_page(
    client: &reqwest::Client,
    token: &str,
    page: &str,
    depth: usize,
    endpoints: &Endpoints,
) -> Result<String, String> {
    if depth > 4 {
        return Ok(String::new());
    }
    let mut out = String::new();
    let mut cursor: Option<String> = None;
    loop {
        let mut url = format!(
            "{}/v1/blocks/{page}/children?page_size=100",
            endpoints.notion.trim_end_matches('/')
        );
        if let Some(c) = &cursor {
            url.push_str(&format!("&start_cursor={c}"));
        }
        let resp = client
            .get(&url)
            .bearer_auth(token)
            .header("Notion-Version", NOTION_VERSION)
            .send()
            .await
            .map_err(|e| format!("notion blocks: {e}"))?;
        if !resp.status().is_success() {
            return Err(format!("notion blocks: HTTP {}", resp.status()));
        }
        let value: Value = resp.json().await.map_err(|e| format!("notion blocks: {e}"))?;
        for block in value["results"].as_array().cloned().unwrap_or_default() {
            let kind = block["type"].as_str().unwrap_or("");
            let payload = &block[kind];
            let rich = payload["rich_text"]
                .as_array()
                .map(|parts| {
                    parts
                        .iter()
                        .filter_map(|p| p["plain_text"].as_str())
                        .collect::<String>()
                })
                .unwrap_or_default();
            match kind {
                "heading_1" => out.push_str(&format!("# {rich}\n\n")),
                "heading_2" => out.push_str(&format!("## {rich}\n\n")),
                "heading_3" => out.push_str(&format!("### {rich}\n\n")),
                "bulleted_list_item" => out.push_str(&format!("- {rich}\n")),
                "numbered_list_item" => out.push_str(&format!("1. {rich}\n")),
                "to_do" => out.push_str(&format!(
                    "- [{}] {rich}\n",
                    if payload["checked"].as_bool() == Some(true) { "x" } else { " " }
                )),
                "code" => out.push_str(&format!("```\n{rich}\n```\n\n")),
                "quote" => out.push_str(&format!("> {rich}\n\n")),
                "paragraph" => out.push_str(&format!("{rich}\n\n")),
                _ => {}
            }
            if block["has_children"].as_bool() == Some(true)
                && let Some(id) = block["id"].as_str()
            {
                let nested = Box::pin(render_page(client, token, id, depth + 1, endpoints)).await?;
                out.push_str(&nested);
            }
        }
        let next = value["next_cursor"].as_str().map(str::to_string);
        if value["has_more"].as_bool() != Some(true) || next.is_none() {
            break;
        }
        cursor = next;
    }
    Ok(out)
}
