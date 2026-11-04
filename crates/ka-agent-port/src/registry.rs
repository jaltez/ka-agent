//! The models.dev registry, pure: parse, cache files, per-vendor merge
//! and the grouped listing the chat pickers show. Split out of the app's
//! `models_live` so the runtime (and every channel through it) can serve
//! `/model` listings; the app keeps the fetch and the one-request probe.
//!
//! Registry: `https://models.dev/api.json` (no auth), shape
//! `{provider_id: {"models": {model_id: …}}}`, cached raw (plus a
//! fetched-at marker) for 24 h under `<data root>/cache/` — the cache
//! files live here, the network does not.
//!
//! Everything here is best-effort: no function raises an error surface
//! of its own.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::catalog;

pub const CACHE_TTL_MS: i64 = 24 * 3_600_000;

/// Registry provider id → ka vendor prefix. Several registry ids share
/// one ka vendor (the registry lists the Zen gateway as "opencode" and
/// the Go plan as "opencode-go"; the Zhipu company as "zai"/"zhipuai"
/// with a separate coding-plan endpoint; Moonshot and Alibaba under
/// their company names). Every other registry entry (and every ka
/// vendor absent from the registry — the local servers) stays
/// catalog-only.
fn ka_vendor(registry_id: &str) -> Option<&'static str> {
    match registry_id {
        // The gateway lists both the Zen pool ("opencode") and the Go
        // subscription plan ("opencode-go"); ka registers the plan.
        "opencode" | "opencode-go" => Some("opencode-go"),
        "anthropic" => Some("anthropic"),
        "openai" => Some("openai"),
        "google" => Some("google"),
        "zai" | "zhipuai" => Some("zai"),
        // The coding-plan endpoint is its own ka vendor.
        "zai-coding-plan" | "zhipuai-coding-plan" => Some("zai-plan"),
        "deepseek" => Some("deepseek"),
        "openrouter" => Some("openrouter"),
        "mistral" => Some("mistral"),
        "groq" => Some("groq"),
        "cerebras" => Some("cerebras"),
        "xai" => Some("xai"),
        "nvidia" => Some("nvidia"),
        // Company ids the registry uses for the ka vendors below.
        "moonshotai" | "moonshotai-cn" => Some("moonshot"),
        "alibaba" | "alibaba-cn" => Some("qwen"),
        "togetherai" => Some("together"),
        "fireworks-ai" => Some("fireworks"),
        _ => None,
    }
}

/// One registry snapshot: per-vendor model ids, when they were
/// learned, and whether that was this process or an earlier cache.
#[derive(Debug, Clone, Default)]
pub struct Registry {
    /// ka vendor → registry model ids (bare — no vendor prefix,
    /// sorted). Invariant: every list is non-empty and sorted.
    pub models: BTreeMap<String, Vec<String>>,
    pub fetched_at_ms: i64,
    pub from_cache: bool,
}

/// `<data root>/cache/models-dev.json` — the raw registry body.
pub fn cache_path(root: &Path) -> PathBuf {
    root.join("cache").join("models-dev.json")
}

/// The fetched-at marker sitting next to the cache.
fn marker_path(root: &Path) -> PathBuf {
    root.join("cache").join("models-dev.json.at")
}

pub fn read_cache(root: &Path) -> Option<(Vec<u8>, i64)> {
    let body = std::fs::read(cache_path(root)).ok()?;
    let at = std::fs::read_to_string(marker_path(root))
        .ok()
        .and_then(|t| t.trim().parse::<i64>().ok())?;
    Some((body, at))
}

pub fn write_cache(root: &Path, body: &[u8], at: i64) {
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
pub fn snapshot(body: &[u8], at: i64, from_cache: bool) -> Registry {
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

/// The one-line footer under a provider's model list: where the live
/// half came from, or that the provider is catalog-only. With no
/// registry snapshot at all (offline, nothing cached) the line says
/// so instead of passing the pinned catalog off as current; a cache
/// past its TTL that a refetch failed to replace says its age.
pub fn footer(reg: Option<&Registry>, vendor: &str) -> String {
    let Some(reg) = reg else {
        return "catalog only (live registry unreachable)".to_string();
    };
    match reg.models.get(vendor) {
        Some(ids) => {
            let hhmm = jiff::Timestamp::from_millisecond(reg.fetched_at_ms)
                .map(|t| t.to_zoned(jiff::tz::TimeZone::system()))
                .map(|z| z.strftime("%H:%M").to_string())
                .unwrap_or_else(|_| "??:??".to_string());
            // A snapshot this old is only ever served after a failed
            // refetch (a fresh cache is returned without one), so it
            // must not dress up as live data.
            if reg.from_cache && ka_agent_core::now_ms() - reg.fetched_at_ms >= CACHE_TTL_MS {
                return format!("stale (cached {hhmm})");
            }
            let cached = if reg.from_cache { " (cached)" } else { "" };
            format!("live {} from models.dev {hhmm}{cached}", ids.len())
        }
        None => "catalog only".to_string(),
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

/// The picker variant of [`merged`]: the recommended model first even
/// when a registry snapshot is present (registry lists are
/// alphabetical, so their first id is not a good default).
pub fn merged_preferred(vendor: &str, reg: Option<&Registry>) -> Vec<String> {
    let mut list = merged(vendor, reg);
    if let Some(d) = catalog::default_model(vendor)
        && let Some(pos) = list.iter().position(|m| m == d)
        && pos > 0
    {
        let recommended = list.remove(pos);
        list.insert(0, recommended);
    }
    list
}

/// One provider's slice of the interactive `/model` listing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderModels {
    /// ka vendor name (the selector prefix).
    pub name: String,
    /// The one-line note the setup picker shows.
    pub note: String,
    /// Full selectors, recommended model first.
    pub models: Vec<String>,
}

/// Everything `/model` with no argument shows: the current model plus
/// every provider that has models to offer, in display order.
#[derive(Debug, Clone, Default)]
pub struct ModelsListing {
    pub current: Option<String>,
    pub providers: Vec<ProviderModels>,
}

impl ModelsListing {
    /// The provider whose models include the current selector, when
    /// the current model is set and known.
    pub fn current_vendor(&self) -> Option<&str> {
        let current = self.current.as_deref()?;
        self.providers
            .iter()
            .find(|p| current.starts_with(&format!("{}/", p.name)))
            .map(|p| p.name.as_str())
    }
}

/// The grouped listing for the interactive `/model`: every provider in
/// display order with its models (recommended first), plus the current
/// selector. Providers with no models at all are left out.
pub fn listing(reg: Option<&Registry>, current: Option<&str>) -> ModelsListing {
    let providers = catalog::providers()
        .into_iter()
        .map(|p| ProviderModels {
            name: p.name.to_string(),
            note: p.note.to_string(),
            models: merged_preferred(p.name, reg),
        })
        .filter(|p| !p.models.is_empty())
        .collect();
    ModelsListing {
        current: current.map(str::to_string),
        providers,
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[test]
    fn registry_provider_ids_map_to_ka_vendors() {
        assert_eq!(ka_vendor("opencode"), Some("opencode-go"));
        assert_eq!(ka_vendor("opencode-go"), Some("opencode-go"));
        for id in [
            "anthropic",
            "openai",
            "google",
            "zai",
            "deepseek",
            "openrouter",
            "mistral",
            "groq",
            "cerebras",
            "xai",
            "nvidia",
        ] {
            assert_eq!(ka_vendor(id), Some(id));
        }
        // Registry company ids alias onto the ka vendor names.
        assert_eq!(ka_vendor("zhipuai"), Some("zai"));
        assert_eq!(ka_vendor("zai-coding-plan"), Some("zai-plan"));
        assert_eq!(ka_vendor("zhipuai-coding-plan"), Some("zai-plan"));
        assert_eq!(ka_vendor("moonshotai"), Some("moonshot"));
        assert_eq!(ka_vendor("moonshotai-cn"), Some("moonshot"));
        assert_eq!(ka_vendor("alibaba"), Some("qwen"));
        assert_eq!(ka_vendor("alibaba-cn"), Some("qwen"));
        assert_eq!(ka_vendor("togetherai"), Some("together"));
        assert_eq!(ka_vendor("fireworks-ai"), Some("fireworks"));
        // Unknown to ka: skipped, never merged.
        assert_eq!(ka_vendor("azure"), None);
        assert_eq!(ka_vendor(""), None);
    }

    #[test]
    fn a_vendor_with_no_catalog_rows_lists_registry_models() {
        // The pinned catalog carries no groq rows; the live registry is
        // the only source of models, and the setup picker must not be
        // empty for a provider it offers.
        assert!(catalog::catalog_models("groq").is_empty());
        let body = br#"{"groq": {"models": {"llama-3.3-70b-versatile": {}, "gpt-oss-120b": {}}}}"#;
        let reg = Registry {
            models: parse_registry(body),
            fetched_at_ms: 0,
            from_cache: false,
        };
        assert_eq!(
            merged("groq", Some(&reg)),
            vec![
                "groq/gpt-oss-120b".to_string(),
                "groq/llama-3.3-70b-versatile".to_string()
            ]
        );
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
        let catalog_only = catalog
            .iter()
            .find(|m| !list.contains(m))
            .cloned()
            .unwrap_or_default();
        if !catalog_only.is_empty() {
            let at = list.iter().position(|m| *m == catalog_only).unwrap();
            assert!(at >= 2, "{catalog_only} must follow the registry block");
        }
    }

    #[test]
    fn preferred_moves_the_recommended_model_to_the_front() {
        let reg = Registry {
            models: BTreeMap::from([(
                "anthropic".to_string(),
                vec!["aaa-first".to_string(), "claude-sonnet-5".to_string()],
            )]),
            fetched_at_ms: 0,
            from_cache: false,
        };
        let list = merged_preferred("anthropic", Some(&reg));
        assert_eq!(list[0], "anthropic/claude-sonnet-5");
        assert_eq!(list[1], "anthropic/aaa-first");
    }

    #[test]
    fn listing_groups_providers_in_display_order_with_the_current() {
        let reg = Registry {
            models: BTreeMap::from([("groq".to_string(), vec!["gpt-oss-120b".to_string()])]),
            fetched_at_ms: 0,
            from_cache: false,
        };
        let l = listing(Some(&reg), Some("anthropic/claude-sonnet-5"));
        assert_eq!(l.current.as_deref(), Some("anthropic/claude-sonnet-5"));
        assert_eq!(l.current_vendor(), Some("anthropic"));
        let names: Vec<&str> = l.providers.iter().map(|p| p.name.as_str()).collect();
        // Display order (vendor_rank): the official providers first. The
        // pinned catalog carries no google rows, so google only appears
        // when the registry lists it.
        assert_eq!(&names[..3], &["anthropic", "openai", "zai"]);
        assert!(!names.contains(&"google"));
        // A vendor with no catalog rows appears with its live models ...
        let groq = l.providers.iter().find(|p| p.name == "groq").unwrap();
        // The recommended default leads even without catalog rows.
        assert_eq!(
            groq.models,
            vec![
                "groq/llama-3.3-70b-versatile".to_string(),
                "groq/gpt-oss-120b".to_string()
            ]
        );
        // ... and vendors with nothing to offer are left out (no
        // registry entry, no catalog rows).
        assert!(!names.contains(&"lmstudio"));

        // With google in the registry it slots into rank order.
        let reg = Registry {
            models: BTreeMap::from([
                ("groq".to_string(), vec!["gpt-oss-120b".to_string()]),
                ("google".to_string(), vec!["gemini-3-pro".to_string()]),
            ]),
            fetched_at_ms: 0,
            from_cache: false,
        };
        let l = listing(Some(&reg), None);
        let names: Vec<&str> = l.providers.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(&names[..4], &["anthropic", "openai", "google", "zai"]);
        assert_eq!(l.current_vendor(), None);
    }

    #[test]
    fn footer_says_where_the_live_half_came_from() {
        let now = ka_agent_core::now_ms();
        let reg = |at: i64, from_cache: bool| Registry {
            models: BTreeMap::from([("opencode-go".to_string(), vec!["glm-5.3".to_string()])]),
            fetched_at_ms: at,
            from_cache,
        };
        let fresh = footer(Some(&reg(now, false)), "opencode-go");
        assert!(fresh.starts_with("live 1 from models.dev "), "{fresh}");
        assert!(!fresh.contains("(cached)"), "{fresh}");
        let cached = footer(Some(&reg(now - 1_000, true)), "opencode-go");
        assert!(cached.ends_with(" (cached)"), "{cached}");

        // A cache past its TTL only exists after a failed refetch: it
        // says its age instead of passing for fresh.
        let stale = footer(Some(&reg(now - CACHE_TTL_MS - 1_000, true)), "opencode-go");
        assert!(stale.starts_with("stale (cached "), "{stale}");
        assert!(!stale.contains("models.dev"), "{stale}");

        // A vendor the registry does not list says so — but only when
        // a snapshot exists at all.
        let with_snapshot = reg(now, false);
        assert_eq!(footer(Some(&with_snapshot), "ollama"), "catalog only");
        // No snapshot: the footer names the pinned catalog and says
        // the live registry did not answer.
        assert_eq!(
            footer(None, "opencode-go"),
            "catalog only (live registry unreachable)"
        );
    }
}
