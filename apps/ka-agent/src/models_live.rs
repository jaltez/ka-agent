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
//! The registry's pure half (parse, cache files, per-vendor merge, the
//! grouped listing the chat pickers show) lives in
//! `ka_agent_port::registry` so the runtime can serve `/model` on every
//! surface; this module keeps the fetch (`https://models.dev/api.json`,
//! no auth, 24 h cache under `<data root>/cache/`) and the probe.
//! `KA_AGENT_MODELS_REGISTRY` overrides the URL; `KA_AGENT_PROBE_URL`
//! overrides the probe's base URL — both for fixture servers in tests.

use std::path::Path;
use std::time::Duration;

use ka_agent_port::catalog::{self, ProviderInfo, WireKind};
use ka_agent_port::registry;

pub use ka_agent_port::registry::{Registry, footer, merged};

const DEFAULT_REGISTRY_URL: &str = "https://models.dev/api.json";
const FETCH_TIMEOUT: Duration = Duration::from_secs(5);
/// A cold model can queue at the gateway before the first byte; the
/// probe's single request gets more rope than the listing fetch.
const PROBE_TIMEOUT: Duration = Duration::from_secs(15);

/// The registry URL (`KA_AGENT_MODELS_REGISTRY` overrides the default —
/// tests point it at a fixture server).
fn registry_url() -> String {
    std::env::var("KA_AGENT_MODELS_REGISTRY")
        .ok()
        .filter(|u| !u.is_empty())
        .unwrap_or_else(|| DEFAULT_REGISTRY_URL.to_string())
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
    let cached = registry::read_cache(root);
    if !force
        && let Some((body, at)) = &cached
        && ka_agent_core::now_ms() - at < registry::CACHE_TTL_MS
    {
        return Some(registry::snapshot(body, *at, true));
    }
    match fetch_registry(url) {
        // Only a body that is valid JSON is worth caching (a captive
        // portal's HTML must not poison the cache for 24 h).
        Ok(body) if serde_json::from_slice::<serde_json::Value>(&body).is_ok() => {
            let at = ka_agent_core::now_ms();
            registry::write_cache(root, &body, at);
            return Some(registry::snapshot(&body, at, false));
        }
        _ => {}
    }
    cached.map(|(body, at)| registry::snapshot(&body, at, true))
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
    /// doctor's Report — supplies the mark). `key_env` is the
    /// provider's key variable when it has one: a not-probed model
    /// names it, so the remedy is the same one command as everywhere
    /// else a missing key is reported.
    pub fn line(&self, model: &str, key_env: Option<&str>) -> String {
        match self {
            ProbeOutcome::Answers => format!("{model} answers"),
            ProbeOutcome::Refused(msg) => format!("{model}: {msg}"),
            ProbeOutcome::NotProbed => match key_env {
                Some(env) => format!("{model} not probed (no key) — ka-agent key set {env}"),
                None => format!("{model} not probed (no key)"),
            },
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
    fn not_probed_line_names_the_key_env_to_set() {
        // A keyed provider names its variable, in the same wording as
        // the model-switch and preflight key warnings.
        assert_eq!(
            ProbeOutcome::NotProbed.line("claude-sonnet-4-5", Some("ANTHROPIC_API_KEY")),
            "claude-sonnet-4-5 not probed (no key) — ka-agent key set ANTHROPIC_API_KEY"
        );
        // A keyless provider has no variable to name.
        assert_eq!(
            ProbeOutcome::NotProbed.line("glm-5.3", None),
            "glm-5.3 not probed (no key)"
        );
        // The hint only concerns the not-probed outcome.
        assert_eq!(ProbeOutcome::Answers.line("m", Some("K")), "m answers");
        assert_eq!(
            ProbeOutcome::Refused("HTTP 503".to_string()).line("m", Some("K")),
            "m: HTTP 503"
        );
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
        registry::write_cache(&root, body, ka_agent_core::now_ms() - 1_000);
        let fresh = load_from(&root, false, gone).unwrap();
        assert!(fresh.from_cache);
        assert!(fresh.models.contains_key("opencode-go"));
        // Stale cache + unreachable registry: still served, marked cached.
        registry::write_cache(
            &root,
            body,
            ka_agent_core::now_ms() - registry::CACHE_TTL_MS - 1_000,
        );
        let stale = load_from(&root, false, gone).unwrap();
        assert!(stale.from_cache, "a dead network never hides the cache");
        // A forced refetch that fails still shows the cache.
        registry::write_cache(&root, body, ka_agent_core::now_ms());
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
