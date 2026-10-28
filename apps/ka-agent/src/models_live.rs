//! Live model knowledge beyond the pinned ka-harness catalog: the
//! models.dev community registry (new models appear in `ka-agent
//! models` and the setup picker without a code change) and a
//! one-request model probe (`models --probe`, `doctor --online`) that
//! catches what no registry can — models the gateway has stopped
//! serving while the registry still lists them.
//!
//! Everything here is best-effort: a fetch that fails falls back to
//! the cache, and with no cache at all callers show exactly the
//! catalog. No function raises an error surface of its own.
//!
//! Registry: `https://models.dev/api.json` (no auth), shape
//! `{provider_id: {"models": {model_id: …}}}`, cached raw (plus a
//! fetched-at marker) for 24 h under `<data root>/cache/`.
//! `KA_AGENT_MODELS_REGISTRY` overrides the URL; `KA_AGENT_PROBE_URL`
//! overrides the probe's base URL — both for fixture servers in tests.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use ka_agent_port::catalog::{self, ProviderInfo, WireKind};

const DEFAULT_REGISTRY_URL: &str = "https://models.dev/api.json";
const FETCH_TIMEOUT: Duration = Duration::from_secs(5);
/// A cold model can queue at the gateway before the first byte; the
/// probe's single request gets more rope than the listing fetch.
const PROBE_TIMEOUT: Duration = Duration::from_secs(15);
const CACHE_TTL_MS: i64 = 24 * 3_600_000;

/// Registry provider id → ka vendor prefix. Only these vendors are
/// merged; every other registry entry (and every ka vendor absent from
/// the registry) stays catalog-only.
fn ka_vendor(registry_id: &str) -> Option<&'static str> {
    match registry_id {
        // The registry lists the OpenCode gateway as "opencode"; ka
        // registers the Go subscription plan as "opencode-go".
        "opencode" => Some("opencode-go"),
        "anthropic" => Some("anthropic"),
        "openai" => Some("openai"),
        "google" => Some("google"),
        "zai" => Some("zai"),
        "deepseek" => Some("deepseek"),
        "openrouter" => Some("openrouter"),
        _ => None,
    }
}

/// The registry URL (`KA_AGENT_MODELS_REGISTRY` overrides the default —
/// tests point it at a fixture server).
fn registry_url() -> String {
    std::env::var("KA_AGENT_MODELS_REGISTRY")
        .ok()
        .filter(|u| !u.is_empty())
        .unwrap_or_else(|| DEFAULT_REGISTRY_URL.to_string())
}

/// One registry snapshot: per-vendor model ids, when they were
/// learned, and whether that was this process or an earlier cache.
#[derive(Debug, Clone, Default)]
pub struct Registry {
    /// ka vendor → registry model ids (bare — no vendor prefix,
    /// sorted). Invariant: every list is non-empty and sorted.
    pub models: BTreeMap<String, Vec<String>>,
    fetched_at_ms: i64,
    from_cache: bool,
}

/// `<data root>/cache/models-dev.json` — the raw registry body.
pub fn cache_path(root: &Path) -> PathBuf {
    root.join("cache").join("models-dev.json")
}

/// The fetched-at marker sitting next to the cache.
fn marker_path(root: &Path) -> PathBuf {
    root.join("cache").join("models-dev.json.at")
}

fn read_cache(root: &Path) -> Option<(Vec<u8>, i64)> {
    let body = std::fs::read(cache_path(root)).ok()?;
    let at = std::fs::read_to_string(marker_path(root))
        .ok()
        .and_then(|t| t.trim().parse::<i64>().ok())?;
    Some((body, at))
}

fn write_cache(root: &Path, body: &[u8], at: i64) {
    if std::fs::create_dir_all(root.join("cache")).is_err() {
        return;
    }
    // Best-effort: a read-only root only costs the next run a refetch.
    let _ = std::fs::write(cache_path(root), body);
    let _ = std::fs::write(marker_path(root), at.to_string());
}

/// Build a snapshot from a raw body (`None`-equivalent: an empty model
/// map when the body parses to nothing). `from_cache` marks one read
/// back from disk rather than fetched by this process.
fn snapshot(body: &[u8], at: i64, from_cache: bool) -> Registry {
    Registry {
        models: parse_registry(body),
        fetched_at_ms: at,
        from_cache,
    }
}

/// Parse the registry document into per-ka-vendor model id lists:
/// `{provider_id: {"models": {model_id: …}}}`; providers ka does not
/// know are skipped. Invalid JSON → empty map (callers treat that as
/// "no data"). Sorts and dedups each list; vendors with nothing left
/// are dropped.
pub fn parse_registry(body: &[u8]) -> BTreeMap<String, Vec<String>> {
    let mut out: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let Ok(doc) = serde_json::from_slice::<serde_json::Value>(body) else {
        return out;
    };
    let Some(providers) = doc.as_object() else {
        return out;
    };
    for (pid, entry) in providers {
        let Some(vendor) = ka_vendor(pid) else {
            continue;
        };
        let Some(models) = entry.get("models").and_then(|m| m.as_object()) else {
            continue;
        };
        out.entry(vendor.to_string())
            .or_default()
            .extend(models.keys().filter(|id| !id.is_empty()).cloned());
    }
    for ids in out.values_mut() {
        ids.sort();
        ids.dedup();
    }
    out.retain(|_, ids| !ids.is_empty());
    out
}

fn fetch_registry(url: &str) -> Result<Vec<u8>, String> {
    // reqwest::blocking builds its own runtime, which must not be
    // dropped inside an async context (setup's model step runs one):
    // do the fetch on a plain thread and join it.
    let url = url.to_string();
    std::thread::spawn(move || {
        let resp = reqwest::blocking::Client::new()
            .get(url)
            .timeout(FETCH_TIMEOUT)
            .header("User-Agent", "ka-agent")
            .send()
            .map_err(|e| e.to_string())?;
        let status = resp.status();
        let body = resp.bytes().map_err(|e| e.to_string())?;
        if !status.is_success() {
            return Err(format!("HTTP {status}"));
        }
        Ok(body.to_vec())
    })
    .join()
    .map_err(|_| "registry fetch thread panicked".to_string())?
}

/// Load the registry: a cached snapshot younger than 24 h, else one
/// fetch (`force` refetches). Any failure — network, HTTP, unparsable
/// body — falls back to the cache however stale it is; with no cache
/// at all the answer is `None` and callers show the catalog as today.
pub fn load(root: &Path, force: bool) -> Option<Registry> {
    load_from(root, force, &registry_url())
}

fn load_from(root: &Path, force: bool, url: &str) -> Option<Registry> {
    let cached = read_cache(root);
    if !force
        && let Some((body, at)) = &cached
        && ka_agent_core::now_ms() - at < CACHE_TTL_MS
    {
        return Some(snapshot(body, *at, true));
    }
    match fetch_registry(url) {
        // Only a body that is valid JSON is worth caching (a captive
        // portal's HTML must not poison the cache for 24 h).
        Ok(body) if serde_json::from_slice::<serde_json::Value>(&body).is_ok() => {
            let at = ka_agent_core::now_ms();
            write_cache(root, &body, at);
            return Some(snapshot(&body, at, false));
        }
        _ => {}
    }
    cached.map(|(body, at)| snapshot(&body, at, true))
}

/// The one-line footer under a provider's model list: where the live
/// half came from, or that the provider is catalog-only. `None` when
/// no registry snapshot exists at all — the listing is then exactly
/// the old catalog-only output, footer included.
pub fn footer(reg: Option<&Registry>, vendor: &str) -> Option<String> {
    let reg = reg?;
    match reg.models.get(vendor) {
        Some(ids) => {
            let hhmm = jiff::Timestamp::from_millisecond(reg.fetched_at_ms)
                .map(|t| t.to_zoned(jiff::tz::TimeZone::system()))
                .map(|z| z.strftime("%H:%M").to_string())
                .unwrap_or_else(|_| "??:??".to_string());
            let cached = if reg.from_cache { " (cached)" } else { "" };
            Some(format!("live {} from models.dev {hhmm}{cached}", ids.len()))
        }
        None => Some("catalog only".to_string()),
    }
}

/// The model list every surface shows for a vendor: registry models
/// (sorted) first, then catalog models the registry does not list.
/// Without a registry snapshot the order is today's — recommended
/// default first, then catalog order.
pub fn merged(vendor: &str, reg: Option<&Registry>) -> Vec<String> {
    let mut catalog_list = Vec::new();
    if let Some(d) = catalog::default_model(vendor) {
        catalog_list.push(d.to_string());
    }
    for m in catalog::catalog_models(vendor) {
        if !catalog_list.contains(&m) {
            catalog_list.push(m);
        }
    }
    let Some(ids) = reg.and_then(|r| r.models.get(vendor)) else {
        return catalog_list;
    };
    // `ids` is sorted (parse_registry's invariant) and the vendor
    // prefix is constant, so the full selectors keep that order.
    let mut out: Vec<String> = ids.iter().map(|id| format!("{vendor}/{id}")).collect();
    for m in catalog_list {
        if !out.contains(&m) {
            out.push(m);
        }
    }
    out
}

/// The model name that goes on the wire for a selector: the catalog
/// row's override when it has one, else the selector's model part
/// (effort suffix stripped).
fn wire_model(selector: &str) -> String {
    let id = selector.split('@').next().unwrap_or(selector);
    if let Some(d) = catalog::catalog().get(id)
        && let Some(w) = &d.wire_model
    {
        return w.clone();
    }
    id.split_once('/')
        .map_or_else(|| id.to_string(), |(_, model)| model.to_string())
}

/// The chat endpoint a probe posts to — the same URLs ka's own wires
/// build (`{base}/v1/messages` for Anthropic, whose base has no
/// version path; `{base}/chat/completions` for the OpenAI family).
fn probe_url(wire: WireKind, base: &str) -> String {
    let base = base.trim_end_matches('/');
    match wire {
        WireKind::Anthropic => format!("{base}/v1/messages"),
        WireKind::OpenAi => format!("{base}/chat/completions"),
    }
}

/// Build the one-request probe. `base` is the (already overridden)
/// base URL; opencode-go additionally needs its session-routing
/// header — the gateway answers HTTP 400 `MissingSessionID` without it.
fn probe_request(
    client: &reqwest::Client,
    p: &ProviderInfo,
    wire_model: &str,
    key: &str,
    base: &str,
) -> reqwest::RequestBuilder {
    let body = serde_json::json!({
        "model": wire_model,
        "max_tokens": 1,
        "messages": [{"role": "user", "content": "ping"}],
    });
    let mut req = client.post(probe_url(p.wire, base)).json(&body);
    req = match p.wire {
        WireKind::Anthropic => req
            .header("x-api-key", key)
            .header("anthropic-version", "2023-06-01"),
        WireKind::OpenAi => req.bearer_auth(key),
    };
    if p.name == "opencode-go" {
        req = req.header("x-opencode-session", "ka-agent-doctor");
    }
    req
}

/// Outcome of a one-request model probe.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProbeOutcome {
    /// The endpoint accepted the request for the model (HTTP 2xx).
    Answers,
    /// It refused — the message is the gateway's own error text
    /// verbatim when the body carries one (`{"error":{"message":…}}`),
    /// else `HTTP <status>` (or the transport error).
    Refused(String),
    /// No key available; nothing was sent.
    NotProbed,
}

impl ProbeOutcome {
    /// The report line without its ✓/✗/· mark (the caller — or
    /// doctor's Report — supplies the mark).
    pub fn line(&self, model: &str) -> String {
        match self {
            ProbeOutcome::Answers => format!("{model} answers"),
            ProbeOutcome::Refused(msg) => format!("{model}: {msg}"),
            ProbeOutcome::NotProbed => format!("{model} not probed (no key)"),
        }
    }

    /// The mark `models --probe` prints in front of [`Self::line`].
    pub fn mark(&self) -> &'static str {
        match self {
            ProbeOutcome::Answers => "✓",
            ProbeOutcome::Refused(_) => "✗",
            ProbeOutcome::NotProbed => "·",
        }
    }
}

/// The message inside a gateway error body — `error.message`, or a
/// bare top-level `message` — verbatim, capped so a wall of HTML
/// cannot flood the report line.
fn error_message(body: &str) -> Option<String> {
    let doc = serde_json::from_str::<serde_json::Value>(body).ok()?;
    let msg = doc
        .get("error")
        .and_then(|e| e.get("message"))
        .or_else(|| doc.get("message"))?
        .as_str()?
        .trim();
    (!msg.is_empty()).then(|| msg.chars().take(300).collect())
}

/// Probe one model with a single minimal request. The base URL comes
/// from the catalog provider; `KA_AGENT_PROBE_URL` overrides it
/// (tests point it at a fixture server).
pub async fn probe(p: &ProviderInfo, selector: &str, key: Option<&str>) -> ProbeOutcome {
    let model = selector.split('@').next().unwrap_or(selector);
    let Some(key) = key.map(str::trim).filter(|k| !k.is_empty()) else {
        return ProbeOutcome::NotProbed;
    };
    let base = std::env::var("KA_AGENT_PROBE_URL")
        .ok()
        .filter(|u| !u.is_empty())
        .unwrap_or_else(|| p.base_url.to_string());
    let Ok(client) = reqwest::Client::builder().timeout(PROBE_TIMEOUT).build() else {
        return ProbeOutcome::Refused("cannot build HTTP client".to_string());
    };
    let resp = match probe_request(&client, p, &wire_model(model), key, &base)
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => return ProbeOutcome::Refused(e.to_string()),
    };
    if resp.status().is_success() {
        return ProbeOutcome::Answers;
    }
    let status = resp.status().as_u16();
    let body = resp.text().await.unwrap_or_default();
    ProbeOutcome::Refused(error_message(&body).unwrap_or_else(|| format!("HTTP {status}")))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    fn provider(name: &'static str, wire: WireKind, base: &'static str) -> ProviderInfo {
        ProviderInfo {
            name,
            note: "",
            base_url: base,
            key_env: None,
            wire,
            key_url: None,
        }
    }

    #[test]
    fn registry_provider_ids_map_to_ka_vendors() {
        assert_eq!(ka_vendor("opencode"), Some("opencode-go"));
        for id in [
            "anthropic",
            "openai",
            "google",
            "zai",
            "deepseek",
            "openrouter",
        ] {
            assert_eq!(ka_vendor(id), Some(id));
        }
        // Unknown to ka: skipped, never merged.
        assert_eq!(ka_vendor("azure"), None);
        assert_eq!(ka_vendor(""), None);
    }

    #[test]
    fn registry_parse_keeps_known_vendors_sorted_and_skips_the_rest() {
        let body = br#"{
            "opencode": {"models": {"zeta": {}, "alpha": {}, "glm-5.3": {}}},
            "anthropic": {"models": {"claude-sonnet-5": {}}},
            "unknown-vendor": {"models": {"x": {}}},
            "empty": {"models": {}}
        }"#;
        let parsed = parse_registry(body);
        assert_eq!(
            parsed.get("opencode-go").unwrap(),
            &["alpha", "glm-5.3", "zeta"],
            "ids are bare and sorted"
        );
        assert_eq!(
            parsed.get("anthropic").unwrap(),
            &["claude-sonnet-5".to_string()]
        );
        assert!(!parsed.contains_key("unknown-vendor"));
        assert!(
            !parsed.contains_key("empty"),
            "vendors with no models drop out"
        );
        assert_eq!(parsed.len(), 2);
    }

    #[test]
    fn registry_parse_rejects_garbage_quietly() {
        assert!(parse_registry(b"<html>captive portal</html>").is_empty());
        assert!(parse_registry(b"[]").is_empty());
    }

    #[test]
    fn merged_list_puts_registry_models_first_then_catalog_only() {
        let reg = Registry {
            models: BTreeMap::from([(
                "opencode-go".to_string(),
                vec!["new-shiny".to_string(), "glm-5.3".to_string()],
            )]),
            fetched_at_ms: 0,
            from_cache: false,
        };
        let list = merged("opencode-go", Some(&reg));
        assert_eq!(list[0], "opencode-go/new-shiny");
        assert_eq!(list[1], "opencode-go/glm-5.3");
        // Catalog models the registry lists are not repeated after it.
        assert_eq!(
            list.iter().filter(|m| *m == "opencode-go/glm-5.3").count(),
            1
        );
        // A catalog-only model keeps its place after the registry block.
        let catalog = catalog::catalog_models("opencode-go");
        assert!(list.len() > 2, "the catalog contributes its own models");
        assert!(
            list.iter()
                .any(|m| catalog.contains(m) && !list[..2].contains(m)),
            "catalog-only models follow the registry models: {list:?}"
        );

        // No snapshot: today's order — default first, then catalog.
        let plain = merged("opencode-go", None);
        assert_eq!(
            plain.first().map(String::as_str),
            catalog::default_model("opencode-go"),
            "the recommended default leads the catalog-only list"
        );
    }

    #[test]
    fn footer_says_where_the_live_half_came_from() {
        let reg = |from_cache: bool| Registry {
            models: BTreeMap::from([("opencode-go".to_string(), vec!["glm-5.3".to_string()])]),
            fetched_at_ms: 1_700_000_000_000,
            from_cache,
        };
        let fresh = footer(Some(&reg(false)), "opencode-go").unwrap();
        assert!(fresh.starts_with("live 1 from models.dev "), "{fresh}");
        assert!(!fresh.contains("(cached)"), "{fresh}");
        let cached = footer(Some(&reg(true)), "opencode-go").unwrap();
        assert!(cached.ends_with(" (cached)"), "{cached}");

        // A vendor the registry does not list says so — but only when
        // a snapshot exists at all.
        let with_snapshot = reg(false);
        assert_eq!(
            footer(Some(&with_snapshot), "ollama").as_deref(),
            Some("catalog only")
        );
        assert_eq!(footer(None, "opencode-go"), None);
    }

    #[test]
    fn cache_round_trip_and_fallback_rules() {
        let root = std::env::temp_dir().join(format!("ka-models-live-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let body = br#"{"opencode": {"models": {"new-shiny": {}}}}"#;
        // No cache and an unreachable registry: nothing, quietly.
        let gone = "http://127.0.0.1:9/api.json";
        assert!(load_from(&root, false, gone).is_none());
        // A fresh cache is served without any request (URL unreachable).
        write_cache(&root, body, ka_agent_core::now_ms() - 1_000);
        let fresh = load_from(&root, false, gone).unwrap();
        assert!(fresh.from_cache);
        assert!(fresh.models.contains_key("opencode-go"));
        // Stale cache + unreachable registry: still served, marked cached.
        write_cache(&root, body, ka_agent_core::now_ms() - CACHE_TTL_MS - 1_000);
        let stale = load_from(&root, false, gone).unwrap();
        assert!(stale.from_cache, "a dead network never hides the cache");
        // A forced refetch that fails still shows the cache.
        write_cache(&root, body, ka_agent_core::now_ms());
        let forced = load_from(&root, true, gone).unwrap();
        assert!(
            forced.from_cache,
            "a forced refetch that fails still shows the cache"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn wire_model_prefers_the_catalog_override() {
        assert_eq!(wire_model("opencode-go/cat-jam-free@high"), "cat-jam-free");
        // A synthetic model (no catalog row) uses the selector's part.
        assert_eq!(wire_model("openrouter/z-ai/glm-5.3"), "z-ai/glm-5.3");
    }

    #[test]
    fn probe_urls_follow_the_engine_wires() {
        assert_eq!(
            probe_url(WireKind::Anthropic, "https://api.anthropic.com"),
            "https://api.anthropic.com/v1/messages"
        );
        assert_eq!(
            probe_url(
                WireKind::OpenAi,
                "https://opencode.ai/inference/go/openai/v1/"
            ),
            "https://opencode.ai/inference/go/openai/v1/chat/completions"
        );
    }

    #[test]
    fn probe_request_carries_auth_session_header_and_tiny_body() {
        let client = reqwest::Client::new();
        let go = provider(
            "opencode-go",
            WireKind::OpenAi,
            "https://opencode.ai/inference/go/openai/v1",
        );
        let req = probe_request(&client, &go, "cat-jam-free", "sk-k", "http://gw")
            .build()
            .unwrap();
        assert_eq!(req.url().as_str(), "http://gw/chat/completions");
        assert_eq!(
            req.headers()
                .get("x-opencode-session")
                .and_then(|v| v.to_str().ok()),
            Some("ka-agent-doctor")
        );
        assert_eq!(
            req.headers()
                .get("authorization")
                .and_then(|v| v.to_str().ok()),
            Some("Bearer sk-k")
        );
        let body = req.body().and_then(|b| b.as_bytes()).unwrap();
        let sent: serde_json::Value = serde_json::from_slice(body).unwrap();
        assert_eq!(sent["model"], "cat-jam-free");
        assert_eq!(sent["max_tokens"], 1);

        // Anthropic wire: x-api-key + version, /v1/messages.
        let anthropic = provider(
            "anthropic",
            WireKind::Anthropic,
            "https://api.anthropic.com",
        );
        let req = probe_request(&client, &anthropic, "claude-sonnet-5", "sk-a", "http://an")
            .build()
            .unwrap();
        assert_eq!(req.url().as_str(), "http://an/v1/messages");
        assert_eq!(
            req.headers().get("x-api-key").and_then(|v| v.to_str().ok()),
            Some("sk-a")
        );
        assert_eq!(
            req.headers()
                .get("anthropic-version")
                .and_then(|v| v.to_str().ok()),
            Some("2023-06-01")
        );
        // No session header outside opencode-go.
        assert!(req.headers().get("x-opencode-session").is_none());
    }

    #[test]
    fn gateway_error_message_is_surfaced_verbatim() {
        let body = r#"{"error":{"type":"server_error","message":"Upstream request failed: Model is unavailable."}}"#;
        assert_eq!(
            error_message(body).unwrap(),
            "Upstream request failed: Model is unavailable."
        );
        assert_eq!(
            error_message(r#"{"message":"MissingSessionID"}"#).unwrap(),
            "MissingSessionID"
        );
        assert_eq!(error_message("<html>400</html>"), None);
        assert_eq!(error_message(r#"{"error":{"message":""}}"#), None);
    }
}
