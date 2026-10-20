//! Provider catalog, model selectors, the engine's dotenv file and a live
//! credential check — the setup/model UX's only window into `ka_dialect`.
//!
//! Base-URL conventions (straight from `ka_dialect::providers::PROVIDERS`):
//! Anthropic's base has no version path (`https://api.anthropic.com`), so the
//! listing is `{base}/v1/models`; OpenAI-wire bases already end in their
//! version path (`…/v1`, `…/v1beta/openai`, `…/paas/v4`), so the listing is
//! `{base}/models`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use ka_dialect::providers::{self, Provider};
use ka_dialect::{Catalog, Wire};

/// Wire family, as far as credential checks care.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WireKind {
    /// Anthropic Messages (`x-api-key` auth).
    Anthropic,
    /// OpenAI Chat/Responses and compatibles (`Bearer` auth).
    OpenAi,
}

/// One provider, enriched for setup.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderInfo {
    pub name: &'static str,
    pub note: &'static str,
    pub base_url: &'static str,
    pub key_env: Option<&'static str>,
    pub wire: WireKind,
    pub key_url: Option<&'static str>,
}

fn key_url(name: &str) -> Option<&'static str> {
    match name {
        "anthropic" => Some("https://console.anthropic.com/settings/keys"),
        "openai" => Some("https://platform.openai.com/api-keys"),
        "google" => Some("https://aistudio.google.com/apikey"),
        "openrouter" => Some("https://openrouter.ai/keys"),
        // Both plans share one console and one key.
        "opencode" | "opencode-go" => Some("https://opencode.ai/console"),
        _ => None,
    }
}

/// Providers ka-dialect 0.3.0 does not register, served by ka-agent's own
/// dialect rows ([`EXTRA_DIALECTS`]). Order matters for
/// [`provider_for_key_env`]: the first entry wins a shared env var.
const EXTRA_PROVIDERS: &[ProviderInfo] = &[
    ProviderInfo {
        name: "opencode-go",
        note: "OpenCode Go subscription — curated coding models (flat monthly)",
        base_url: "https://opencode.ai/zen/go/v1",
        key_env: Some("OPENCODE_API_KEY"),
        wire: WireKind::OpenAi,
        key_url: Some("https://opencode.ai/console"),
    },
    ProviderInfo {
        name: "opencode",
        note: "OpenCode Zen — curated coding models, pay per token",
        base_url: "https://opencode.ai/zen/v1",
        key_env: Some("OPENCODE_API_KEY"),
        wire: WireKind::OpenAi,
        key_url: Some("https://opencode.ai/console"),
    },
];

fn extra_provider(name: &str) -> Option<&'static ProviderInfo> {
    EXTRA_PROVIDERS.iter().find(|p| p.name == name)
}

/// ka's embedded dialect catalog plus ka-agent's extra providers
/// (`opencode_dialects.toml`). The worker spawns the engine on this catalog;
/// selectors from [`EXTRA_PROVIDERS`] resolve only through it.
pub fn full_catalog() -> Catalog {
    let mut catalog = Catalog::embedded();
    if let Ok(extra) = Catalog::parse(EXTRA_DIALECTS) {
        catalog.overlay(extra);
    }
    catalog
}

/// ka-agent's dialect rows for [`EXTRA_PROVIDERS`] (models.dev-derived).
const EXTRA_DIALECTS: &str = include_str!("opencode_dialects.toml");

fn info(p: &Provider) -> ProviderInfo {
    ProviderInfo {
        name: p.name,
        note: p.note,
        base_url: p.base_url,
        key_env: p.key_env,
        wire: match p.wire {
            Wire::AnthropicMessages => WireKind::Anthropic,
            Wire::OpenaiChat | Wire::OpenaiResponses => WireKind::OpenAi,
        },
        key_url: key_url(p.name),
    }
}

/// Every known provider in display order (`vendor_rank`, stable), with
/// ka-agent's extras slotted between the community tier and local runtimes.
pub fn providers() -> Vec<ProviderInfo> {
    let mut all: Vec<&Provider> = providers::PROVIDERS.iter().collect();
    all.sort_by_key(|p| providers::vendor_rank(p.name));
    let local_starts = all
        .iter()
        .position(|p| providers::vendor_rank(p.name) >= 900)
        .unwrap_or(all.len());
    let mut out: Vec<ProviderInfo> = all[..local_starts].iter().map(|p| info(p)).collect();
    out.extend(EXTRA_PROVIDERS.iter().cloned());
    out.extend(all[local_starts..].iter().map(|p| info(p)));
    out
}

/// Look a provider up by vendor prefix (legacy z.ai aliases accepted).
pub fn provider(name: &str) -> Option<ProviderInfo> {
    match providers::find(name) {
        Some(p) => Some(info(p)),
        None => extra_provider(name).cloned(),
    }
}

/// The first provider (display order) whose key lives in `env`.
pub fn provider_for_key_env(env: &str) -> Option<ProviderInfo> {
    providers().into_iter().find(|p| p.key_env == Some(env))
}

/// The recommended model for a vendor, when there is one.
pub fn default_model(vendor: &str) -> Option<&'static str> {
    match vendor {
        "anthropic" => Some("anthropic/claude-sonnet-5"),
        "openai" => Some("openai/gpt-5.1"),
        "zai" => Some("zai/glm-5.3"),
        "deepseek" => Some("deepseek/deepseek-flash"),
        "openrouter" => Some("openrouter/z-ai/glm-5.3"),
        "ollama" => Some("ollama/qwen3-32b"),
        "opencode-go" => Some("opencode-go/glm-5.3"),
        "opencode" => Some("opencode/glm-5.3"),
        _ => None,
    }
}

/// Catalog model ids for a vendor (`vendor/…`), in catalog order.
pub fn catalog_models(vendor: &str) -> Vec<String> {
    let prefix = format!("{vendor}/");
    full_catalog()
        .dialects
        .into_keys()
        .filter(|k| k.starts_with(&prefix))
        .collect()
}

/// A validated model selector.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelectorInfo {
    /// `vendor/model[@effort]`, normalized.
    pub id: String,
    pub vendor: String,
    pub key_env: Option<String>,
    pub in_catalog: bool,
}

/// Accept a selector that the catalog knows or a known provider can synthesize.
pub fn validate_selector(sel: &str) -> Result<SelectorInfo, String> {
    let unknown =
        || format!("unknown model {sel:?} — use provider/model, e.g. anthropic/claude-sonnet-5");
    let parsed = ka_dialect::parse_selector(sel.trim()).map_err(|_| unknown())?;
    let model_id = parsed.model_id();
    let id = match &parsed.effort {
        Some(e) => format!("{model_id}@{e}"),
        None => model_id.clone(),
    };
    let catalog = full_catalog();
    if let Some(d) = catalog.get(&model_id) {
        let key_env = d.api_key_env.clone().or_else(|| {
            providers::find(&parsed.vendor).and_then(|p| p.key_env.map(str::to_string))
        });
        return Ok(SelectorInfo {
            id,
            vendor: parsed.vendor,
            key_env,
            in_catalog: true,
        });
    }
    match providers::find(&parsed.vendor) {
        Some(p) => Ok(SelectorInfo {
            id,
            vendor: parsed.vendor,
            key_env: p.key_env.map(str::to_string),
            in_catalog: false,
        }),
        // ka-agent's own providers synthesize against their dialect rows.
        None => extra_provider(&parsed.vendor)
            .map(|p| SelectorInfo {
                id,
                vendor: parsed.vendor,
                key_env: p.key_env.map(str::to_string),
                in_catalog: false,
            })
            .ok_or_else(unknown),
    }
}

/// `<engine_home>/.config/ka/.env` — where ka resolves provider keys.
pub fn engine_env_path(engine_home: &Path) -> PathBuf {
    engine_home.join(".config").join("ka").join(".env")
}

/// Parse a dotenv file (`KEY=VALUE`, optional `export `, quotes stripped,
/// `#` comments, empty values skipped). Missing/unreadable → empty.
pub fn read_env_file(path: &Path) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    let Ok(text) = std::fs::read_to_string(path) else {
        return out;
    };
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let line = line.strip_prefix("export ").unwrap_or(line).trim_start();
        let Some((k, v)) = line.split_once('=') else {
            continue;
        };
        let k = k.trim();
        let v = v.trim();
        let v = if v.len() >= 2
            && ((v.starts_with('"') && v.ends_with('"'))
                || (v.starts_with('\'') && v.ends_with('\'')))
        {
            &v[1..v.len() - 1]
        } else {
            v
        };
        if k.is_empty() || v.is_empty() {
            continue;
        }
        out.insert(k.to_string(), v.to_string());
    }
    out
}

/// Upsert one `NAME=value` line (ka's `save_api_key` semantics: replace the
/// existing line, append otherwise, 0600, single trailing newline).
pub fn upsert_env_file(path: &Path, name: &str, value: &str) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let pattern = format!("{name}=");
    let mut lines: Vec<String> = std::fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .map(str::to_string)
        .collect();
    match lines
        .iter_mut()
        .find(|l| l.trim_start().starts_with(&pattern))
    {
        Some(slot) => *slot = format!("{name}={value}"),
        None => lines.push(format!("{name}={value}")),
    }
    let mut text = lines.join("\n");
    text.push('\n');
    std::fs::write(path, text)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    }
    Ok(())
}

/// Whether the key a selector needs is available.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeyState {
    Present,
    /// The env var that is missing.
    Missing(String),
    /// The provider needs no key.
    Keyless,
}

pub fn key_state(sel: &SelectorInfo, env: &BTreeMap<String, String>) -> KeyState {
    match &sel.key_env {
        None => KeyState::Keyless,
        Some(k) if env.get(k).is_some_and(|v| !v.is_empty()) => KeyState::Present,
        Some(k) => KeyState::Missing(k.clone()),
    }
}

/// Outcome of a live credential probe.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CredentialCheck {
    Ok { models: Vec<String> },
    Rejected { status: u16, message: String },
    NoListing { status: u16 },
    Unreachable { message: String },
}

const CHECK_TIMEOUT: Duration = Duration::from_secs(8);

fn first_chars(s: &str, n: usize) -> String {
    s.trim().chars().take(n).collect()
}

/// List models at the provider with `key` to prove the key and endpoint work.
pub async fn check_credentials(p: &ProviderInfo, key: Option<&str>) -> CredentialCheck {
    check_at(p.wire, p.base_url, key).await
}

async fn check_at(wire: WireKind, base_url: &str, key: Option<&str>) -> CredentialCheck {
    let client = match reqwest::Client::builder().timeout(CHECK_TIMEOUT).build() {
        Ok(c) => c,
        Err(e) => {
            return CredentialCheck::Unreachable {
                message: e.to_string(),
            };
        }
    };
    let base = base_url.trim_end_matches('/');
    let req = match wire {
        WireKind::Anthropic => {
            let mut r = client
                .get(format!("{base}/v1/models"))
                .header("anthropic-version", "2023-06-01");
            if let Some(k) = key {
                r = r.header("x-api-key", k);
            }
            r
        }
        WireKind::OpenAi => {
            let mut r = client.get(format!("{base}/models"));
            if let Some(k) = key {
                r = r.bearer_auth(k);
            }
            r
        }
    };
    let resp = match req.send().await {
        Ok(r) => r,
        Err(e) => {
            return CredentialCheck::Unreachable {
                message: e.to_string(),
            };
        }
    };
    let status = resp.status().as_u16();
    let body = resp.text().await.unwrap_or_default();
    match status {
        200..=299 => {
            let models = serde_json::from_str::<serde_json::Value>(&body)
                .ok()
                .and_then(|v| {
                    v.get("data").and_then(|d| d.as_array()).map(|arr| {
                        arr.iter()
                            .filter_map(|m| m.get("id").and_then(|i| i.as_str()))
                            .map(str::to_string)
                            .collect()
                    })
                })
                .unwrap_or_default();
            CredentialCheck::Ok { models }
        }
        401 | 403 => CredentialCheck::Rejected {
            status,
            message: first_chars(&body, 200),
        },
        404 | 405 => CredentialCheck::NoListing { status },
        _ => CredentialCheck::Unreachable {
            message: format!("HTTP {status}: {}", first_chars(&body, 200)),
        },
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    #[test]
    fn opencode_providers_are_listed_and_resolvable() {
        let names: Vec<&str> = providers().iter().map(|p| p.name).collect();
        assert!(names.contains(&"opencode-go"), "{names:?}");
        assert!(names.contains(&"opencode"), "{names:?}");
        // Extras slot in before local runtimes (ollama is local-tier).
        let go = names.iter().position(|n| *n == "opencode-go").unwrap();
        let ollama = names.iter().position(|n| *n == "ollama").unwrap();
        assert!(go < ollama);

        // One console key backs both plans; the subscription (Go) wins the
        // shared env var for the setup wizard's "key found" default.
        assert_eq!(
            provider_for_key_env("OPENCODE_API_KEY").unwrap().name,
            "opencode-go"
        );

        // The default model resolves and is a real dialect row.
        let d = default_model("opencode-go").unwrap();
        let sel = validate_selector(d).unwrap();
        assert!(sel.in_catalog, "{d} must be a catalog row");
        assert_eq!(sel.key_env.as_deref(), Some("OPENCODE_API_KEY"));

        // Every model the picker lists resolves through the full catalog.
        let go_models = catalog_models("opencode-go");
        assert!(go_models.len() >= 20, "{go_models:?}");
        for m in &go_models {
            assert!(full_catalog().get(m).is_some(), "{m} missing");
        }
        let zen_models = catalog_models("opencode");
        assert!(!zen_models.is_empty());
        for m in &zen_models {
            assert!(full_catalog().get(m).is_some(), "{m} missing");
        }

        // A synthetic (non-catalog) opencode model still validates — the
        // provider exists and the wire is OpenAI-chat.
        let s = validate_selector("opencode-go/custom-model@high").unwrap();
        assert!(!s.in_catalog);
        assert_eq!(s.key_env.as_deref(), Some("OPENCODE_API_KEY"));
    }

    #[test]
    fn default_models_resolve() {
        let cat = full_catalog();
        for p in providers() {
            if let Some(m) = default_model(p.name) {
                let sel = ka_dialect::parse_selector(m).unwrap();
                assert!(
                    cat.get(m).is_some()
                        || providers::find(&sel.vendor).is_some()
                        || extra_provider(&sel.vendor).is_some(),
                    "{m}"
                );
                assert!(validate_selector(m).is_ok(), "{m}");
            }
        }
        assert!(
            Catalog::embedded()
                .get("anthropic/claude-sonnet-5")
                .is_some()
        );
    }

    #[test]
    fn provider_order_and_lookup() {
        let all = providers();
        assert_eq!(all[0].name, "anthropic");
        assert_eq!(all[0].wire, WireKind::Anthropic);
        assert_eq!(all.last().unwrap().key_env, None);
        assert_eq!(provider("zhipu").unwrap().name, "zai");
        assert_eq!(
            provider_for_key_env("OPENAI_API_KEY").unwrap().name,
            "openai"
        );
        assert_eq!(
            provider("openai").unwrap().key_url,
            Some("https://platform.openai.com/api-keys")
        );
        assert!(catalog_models("anthropic").contains(&"anthropic/claude-sonnet-5".to_string()));
    }

    #[test]
    fn selectors() {
        let s = validate_selector("anthropic/claude-sonnet-5").unwrap();
        assert_eq!(s.id, "anthropic/claude-sonnet-5");
        assert_eq!(s.key_env.as_deref(), Some("ANTHROPIC_API_KEY"));
        assert!(s.in_catalog);
        let s = validate_selector("groq/some-model@high").unwrap();
        assert_eq!(s.id, "groq/some-model@high");
        assert!(!s.in_catalog);
        assert_eq!(
            validate_selector("nope/zzz").unwrap_err(),
            "unknown model \"nope/zzz\" — use provider/model, e.g. anthropic/claude-sonnet-5"
        );
        assert!(validate_selector("noslash").is_err());
    }

    #[test]
    fn env_file_parse_and_upsert() {
        let dir = std::env::temp_dir().join(format!("ka-cat-{}", std::process::id()));
        let path = engine_env_path(&dir);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            "# comment\nexport A=1\nB=\"two\"\nC='three'\nEMPTY=\n\nD = 4\n",
        )
        .unwrap();
        let env = read_env_file(&path);
        assert_eq!(env.get("A").unwrap(), "1");
        assert_eq!(env.get("B").unwrap(), "two");
        assert_eq!(env.get("C").unwrap(), "three");
        assert_eq!(env.get("D").unwrap(), "4");
        assert!(!env.contains_key("EMPTY"));
        upsert_env_file(&path, "B", "new").unwrap();
        upsert_env_file(&path, "Z", "z").unwrap();
        let env = read_env_file(&path);
        assert_eq!(env.get("B").unwrap(), "new");
        assert_eq!(env.get("Z").unwrap(), "z");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        assert!(read_env_file(&dir.join("missing")).is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn key_states() {
        let mut env = BTreeMap::new();
        let s = validate_selector("anthropic/claude-sonnet-5").unwrap();
        assert_eq!(
            key_state(&s, &env),
            KeyState::Missing("ANTHROPIC_API_KEY".into())
        );
        env.insert("ANTHROPIC_API_KEY".into(), "k".into());
        assert_eq!(key_state(&s, &env), KeyState::Present);
        let local = validate_selector("lmstudio/whatever").unwrap();
        assert_eq!(key_state(&local, &env), KeyState::Keyless);
    }

    /// One-shot HTTP fake: returns the request head and replies `status`/`body`.
    async fn fake(status: &str, body: &str) -> (String, tokio::task::JoinHandle<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let reply = format!(
            "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        let h = tokio::spawn(async move {
            let (mut s, _) = listener.accept().await.unwrap();
            let mut buf = Vec::new();
            let mut chunk = [0u8; 1024];
            while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
                let n = s.read(&mut chunk).await.unwrap();
                if n == 0 {
                    break;
                }
                buf.extend_from_slice(&chunk[..n]);
            }
            s.write_all(reply.as_bytes()).await.unwrap();
            s.shutdown().await.ok();
            String::from_utf8_lossy(&buf).to_lowercase()
        });
        (format!("http://{addr}"), h)
    }

    #[tokio::test]
    async fn anthropic_headers_and_models() {
        let (base, h) = fake(
            "200 OK",
            r#"{"data":[{"id":"claude-a"},{"id":"claude-b"}]}"#,
        )
        .await;
        let r = check_at(WireKind::Anthropic, &base, Some("sk-x")).await;
        let head = h.await.unwrap();
        assert!(head.starts_with("get /v1/models "), "{head}");
        assert!(head.contains("x-api-key: sk-x"), "{head}");
        assert!(head.contains("anthropic-version: 2023-06-01"), "{head}");
        assert!(!head.contains("authorization"), "{head}");
        assert_eq!(
            r,
            CredentialCheck::Ok {
                models: vec!["claude-a".into(), "claude-b".into()]
            }
        );
    }

    #[tokio::test]
    async fn openai_bearer() {
        let (base, h) = fake("200 OK", "not json").await;
        let r = check_at(WireKind::OpenAi, &format!("{base}/v1"), Some("k")).await;
        let head = h.await.unwrap();
        assert!(head.starts_with("get /v1/models "), "{head}");
        assert!(head.contains("authorization: bearer k"), "{head}");
        assert_eq!(r, CredentialCheck::Ok { models: vec![] });
    }

    #[tokio::test]
    async fn openai_keyless_has_no_auth() {
        let (base, h) = fake("200 OK", r#"{"data":[]}"#).await;
        let _ = check_at(WireKind::OpenAi, &base, None).await;
        assert!(!h.await.unwrap().contains("authorization"));
    }

    #[tokio::test]
    async fn rejected_on_401() {
        let (base, h) = fake("401 Unauthorized", r#"{"error":"bad key"}"#).await;
        let r = check_at(WireKind::OpenAi, &base, Some("k")).await;
        h.await.unwrap();
        assert_eq!(
            r,
            CredentialCheck::Rejected {
                status: 401,
                message: r#"{"error":"bad key"}"#.into()
            }
        );
    }

    #[tokio::test]
    async fn no_listing_on_404() {
        let (base, h) = fake("404 Not Found", "").await;
        let r = check_at(WireKind::Anthropic, &base, Some("k")).await;
        h.await.unwrap();
        assert_eq!(r, CredentialCheck::NoListing { status: 404 });
    }

    #[tokio::test]
    async fn unreachable_on_closed_port() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);
        let r = check_at(WireKind::OpenAi, &format!("http://{addr}"), Some("k")).await;
        assert!(matches!(r, CredentialCheck::Unreachable { .. }), "{r:?}");
    }
}

#[cfg(test)]
#[allow(dead_code)]
mod parse_check_bin {
    #[test]
    fn extra_dialects_parse() {
        match super::Catalog::parse(super::EXTRA_DIALECTS) {
            Ok(c) => assert!(!c.dialects.is_empty()),
            Err(e) => panic!("EXTRA_DIALECTS invalid: {e}"),
        }
    }
}
