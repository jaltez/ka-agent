//! The registry of every `config.toml` key (plan 1.5): one table drives
//! `config schema`, unknown-key suggestions and hot reload. Each entry
//! names the TOML path, its type (with bounds), its serialized default,
//! how a change applies to a running daemon, whether the web panel may
//! edit it, and one doc line. Steps that add config keys register them
//! here in the same step.

/// How a change reaches a running daemon.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Apply {
    /// Takes effect immediately (surface behavior, scheduling, budgets).
    Live,
    /// Live, but shapes the engine: an idle worker restarts on the next
    /// turn (a busy one already carries the old config to its turn's end).
    Worker,
    /// Only a daemon restart applies it (channels bind at startup).
    Restart,
}

impl Apply {
    pub fn label(self) -> &'static str {
        match self {
            Apply::Live => "now",
            Apply::Worker => "next turn",
            Apply::Restart => "restart",
        }
    }
}

/// A key's type, as `config schema` renders it and the web form builds it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Kind {
    Bool,
    Int {
        min: i64,
        max: i64,
    },
    Float {
        min: f64,
    },
    Str,
    Enum(&'static [&'static str]),
    List,
    /// An array-of-tables section (`mcp`, `hooks`): managed as a whole.
    Table,
}

impl Kind {
    pub fn label(self) -> &'static str {
        match self {
            Kind::Bool => "bool",
            Kind::Int { .. } => "int",
            Kind::Float { .. } => "float",
            Kind::Str => "string",
            Kind::Enum(_) => "enum",
            Kind::List => "list",
            Kind::Table => "table",
        }
    }
}

/// One registered key.
pub struct KeySpec {
    /// TOML path, e.g. `budgets.daily_usd`.
    pub path: &'static str,
    pub kind: Kind,
    /// The serialized default (absent sections say so).
    pub default: &'static str,
    pub apply: Apply,
    /// Editable from the web settings page.
    pub web: bool,
    pub doc: &'static str,
}

/// Every leaf key of `DaemonConfig` plus the array-of-tables sections.
/// Order is the `config schema` display order.
pub static KEYS: &[KeySpec] = &[
    KeySpec {
        path: "workspace",
        kind: Kind::Str,
        default: "~/.ka-agent/profiles/default/workspace",
        apply: Apply::Restart,
        web: false,
        doc: "the single directory every engine session runs in",
    },
    KeySpec {
        path: "model",
        kind: Kind::Str,
        default: "unset (ka-agent setup)",
        apply: Apply::Worker,
        web: true,
        doc: "default model selector for new sessions (vendor/model[@effort])",
    },
    KeySpec {
        path: "fallback",
        kind: Kind::List,
        default: "[]",
        apply: Apply::Worker,
        web: true,
        doc: "models tried in order when the main one fails (max 2 hops)",
    },
    KeySpec {
        path: "effort",
        kind: Kind::Enum(&["off", "low", "medium", "high", "max"]),
        default: "unset (model default)",
        apply: Apply::Worker,
        web: true,
        doc: "reasoning effort sent to the engine",
    },
    KeySpec {
        path: "background_model",
        kind: Kind::Str,
        default: "unset (main model)",
        apply: Apply::Worker,
        web: true,
        doc: "model for background turns (crons, briefings, --background asks)",
    },
    KeySpec {
        path: "sandbox",
        kind: Kind::Enum(&["auto", "fs", "off"]),
        default: "auto",
        apply: Apply::Worker,
        web: true,
        doc: "OS sandbox for bash: auto (fs when a backend exists) | fs | off",
    },
    KeySpec {
        path: "busy_messages",
        kind: Kind::Enum(&["steer", "queue"]),
        default: "steer",
        apply: Apply::Live,
        web: true,
        doc: "a message that arrives mid-turn: steer into the reply, or queue for the next turn",
    },
    KeySpec {
        path: "mode",
        kind: Kind::Enum(&["guarded", "accept_edits", "free", "plan"]),
        default: "guarded",
        apply: Apply::Worker,
        web: true,
        doc: "permission mode: guarded asks, accept_edits auto-applies edits, plan is read-only, free asks nothing",
    },
    KeySpec {
        path: "approval_ttl_secs",
        kind: Kind::Int {
            min: 30,
            max: 86_400,
        },
        default: "900",
        apply: Apply::Live,
        web: true,
        doc: "approval card lifetime; deny on expiry",
    },
    KeySpec {
        path: "turn_grace_secs",
        kind: Kind::Int { min: 1, max: 600 },
        default: "10",
        apply: Apply::Live,
        web: false,
        doc: "grace after /stop before SIGTERM/SIGKILL escalation",
    },
    KeySpec {
        path: "worker_idle_timeout_secs",
        kind: Kind::Int {
            min: 0,
            max: 86_400,
        },
        default: "900",
        apply: Apply::Live,
        web: true,
        doc: "stop an idle worker after this many seconds (0 = keep forever)",
    },
    KeySpec {
        path: "worker_restart.backoff_start_ms",
        kind: Kind::Int {
            min: 1,
            max: 60_000,
        },
        default: "1000",
        apply: Apply::Live,
        web: false,
        doc: "first backoff before a worker restart attempt",
    },
    KeySpec {
        path: "worker_restart.backoff_max_ms",
        kind: Kind::Int {
            min: 1,
            max: 600_000,
        },
        default: "60000",
        apply: Apply::Live,
        web: false,
        doc: "backoff ceiling between worker restarts",
    },
    KeySpec {
        path: "worker_restart.crash_window_secs",
        kind: Kind::Int {
            min: 10,
            max: 86_400,
        },
        default: "600",
        apply: Apply::Live,
        web: false,
        doc: "crash-loop window",
    },
    KeySpec {
        path: "worker_restart.crash_max",
        kind: Kind::Int { min: 1, max: 100 },
        default: "5",
        apply: Apply::Live,
        web: false,
        doc: "crashes inside the window before the crash-loop breaker trips",
    },
    KeySpec {
        path: "budgets.daily_usd",
        kind: Kind::Float { min: 0.0 },
        default: "unset",
        apply: Apply::Live,
        web: true,
        doc: "daily spend cap (USD); asks once when reached",
    },
    KeySpec {
        path: "budgets.daily_tokens",
        kind: Kind::Int {
            min: 0,
            max: 1_000_000_000,
        },
        default: "unset",
        apply: Apply::Live,
        web: true,
        doc: "daily token cap",
    },
    KeySpec {
        path: "ka.engine_layer",
        kind: Kind::Str,
        default: "unset",
        apply: Apply::Worker,
        web: false,
        doc: "path to a ka engine layer (ka.toml) merged under ka-agent's own",
    },
    KeySpec {
        path: "telegram.enabled",
        kind: Kind::Bool,
        default: "false",
        apply: Apply::Restart,
        web: false,
        doc: "enable the Telegram channel",
    },
    KeySpec {
        path: "telegram.token_secret",
        kind: Kind::Str,
        default: "unset",
        apply: Apply::Restart,
        web: false,
        doc: "secret-store name of the bot token",
    },
    KeySpec {
        path: "telegram.allow_user_ids",
        kind: Kind::List,
        default: "[]",
        apply: Apply::Restart,
        web: false,
        doc: "deprecated: paired ids, migrated into identities at startup",
    },
    KeySpec {
        path: "telegram.api_base",
        kind: Kind::Str,
        default: "unset (https://api.telegram.org)",
        apply: Apply::Restart,
        web: false,
        doc: "Bot API base override (tests point it at a fake)",
    },
    KeySpec {
        path: "telegram.stream",
        kind: Kind::Bool,
        default: "true",
        apply: Apply::Restart,
        web: false,
        doc: "stream replies as drafts where the API allows",
    },
    KeySpec {
        path: "matrix.enabled",
        kind: Kind::Bool,
        default: "false",
        apply: Apply::Restart,
        web: false,
        doc: "enable the Matrix channel",
    },
    KeySpec {
        path: "matrix.homeserver",
        kind: Kind::Str,
        default: "unset",
        apply: Apply::Restart,
        web: false,
        doc: "homeserver base URL",
    },
    KeySpec {
        path: "matrix.user_id",
        kind: Kind::Str,
        default: "unset",
        apply: Apply::Restart,
        web: false,
        doc: "full Matrix user id of the bot account",
    },
    KeySpec {
        path: "matrix.token_secret",
        kind: Kind::Str,
        default: "unset",
        apply: Apply::Restart,
        web: false,
        doc: "secret-store name of the access token",
    },
    KeySpec {
        path: "timezone",
        kind: Kind::Str,
        default: "system zone",
        apply: Apply::Live,
        web: true,
        doc: "operator IANA timezone (e.g. Europe/Madrid)",
    },
    KeySpec {
        path: "web.enabled",
        kind: Kind::Bool,
        default: "true",
        apply: Apply::Restart,
        web: false,
        doc: "serve the web panel on 127.0.0.1",
    },
    KeySpec {
        path: "web.port",
        kind: Kind::Int {
            min: 1,
            max: 65_535,
        },
        default: "8480",
        apply: Apply::Restart,
        web: false,
        doc: "web panel port",
    },
    KeySpec {
        path: "calendar.digest_time",
        kind: Kind::Str,
        default: "08:00",
        apply: Apply::Live,
        web: true,
        doc: "local clock time of the daily calendar digest (HH:MM)",
    },
    KeySpec {
        path: "search.provider",
        kind: Kind::Enum(&["tavily", "brave", "bing"]),
        default: "unset (search off)",
        apply: Apply::Worker,
        web: true,
        doc: "web-search provider for the engine (section absent = off)",
    },
    KeySpec {
        path: "search.base_url",
        kind: Kind::Str,
        default: "unset",
        apply: Apply::Worker,
        web: false,
        doc: "search API base override",
    },
    KeySpec {
        path: "mcp",
        kind: Kind::Table,
        default: "[]",
        apply: Apply::Worker,
        web: false,
        doc: "operator-declared MCP servers ([[mcp]] array)",
    },
    KeySpec {
        path: "voice.provider",
        kind: Kind::Enum(&["openai", "groq", "local"]),
        default: "unset (voice off)",
        apply: Apply::Restart,
        web: false,
        doc: "voice-note transcription provider",
    },
    KeySpec {
        path: "voice.base_url",
        kind: Kind::Str,
        default: "unset",
        apply: Apply::Restart,
        web: false,
        doc: "transcription API base (required for local)",
    },
    KeySpec {
        path: "voice.model",
        kind: Kind::Str,
        default: "unset",
        apply: Apply::Restart,
        web: false,
        doc: "transcription model",
    },
    KeySpec {
        path: "notify.quiet_hours",
        kind: Kind::Str,
        default: "unset",
        apply: Apply::Live,
        web: true,
        doc: "hold scheduled prompts inside this local window (22:00-07:00)",
    },
    KeySpec {
        path: "approvals.auto_review",
        kind: Kind::Bool,
        default: "false",
        apply: Apply::Worker,
        web: true,
        doc: "a cheap model pre-screens reviewable asks",
    },
];

/// Exact key lookup, else the longest registered table prefix.
pub fn lookup(path: &str) -> Option<&'static KeySpec> {
    if let Some(k) = KEYS.iter().find(|k| k.path == path) {
        return Some(k);
    }
    // longest table prefix: `mcp.<i>.<field>` belongs to the `mcp` spec
    KEYS.iter()
        .filter(|k| k.kind == Kind::Table && path.starts_with(&format!("{}.", k.path)))
        .max_by_key(|k| k.path.len())
}

/// The closest registered path within edit distance 3, when one exists.
pub fn suggest(path: &str) -> Option<&'static str> {
    fn distance(a: &str, b: &str) -> usize {
        // bounded Levenshtein; anything past 3 counts as 4
        let b: Vec<char> = b.chars().collect();
        let mut prev: Vec<usize> = (0..=b.len()).collect();
        for (i, ca) in a.chars().enumerate() {
            let mut cur = vec![i + 1];
            for (j, cb) in b.iter().enumerate() {
                let cost = usize::from(ca != *cb);
                cur.push((prev[j] + cost).min(prev[j + 1] + 1).min(cur[j] + 1));
            }
            prev = cur;
            if prev.iter().min().unwrap_or(&4) > &3 {
                return 4;
            }
        }
        *prev.last().unwrap_or(&4)
    }
    let mut best: Option<(usize, &'static str)> = None;
    for k in KEYS {
        let d = distance(path, k.path);
        if d <= 3 && best.is_none_or(|(bd, _)| d < bd) {
            best = Some((d, k.path));
        }
    }
    best.map(|(_, p)| p)
}

fn value_at<'v>(root: &'v toml::Value, path: &str) -> Option<&'v toml::Value> {
    let mut cur = root;
    for part in path.split('.') {
        cur = cur.get(part)?;
    }
    Some(cur)
}

/// Which registered keys differ between two configs (serialization diff,
pub fn changed(
    old: &crate::config::DaemonConfig,
    new: &crate::config::DaemonConfig,
) -> Vec<&'static KeySpec> {
    let (Ok(old), Ok(new)) = (toml::Value::try_from(old), toml::Value::try_from(new)) else {
        return KEYS.iter().collect();
    };
    KEYS.iter()
        .filter(|k| value_at(&old, k.path) != value_at(&new, k.path))
        .collect()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;
    use crate::config::DaemonConfig;

    /// Walk every leaf of a toml value as `a.b.c` paths.
    fn leaf_paths(v: &toml::Value, prefix: &str, out: &mut Vec<String>) {
        match v {
            toml::Value::Table(t) => {
                for (k, val) in t {
                    leaf_paths(val, &format!("{prefix}{k}."), out);
                }
            }
            toml::Value::Array(a) => {
                if !a.is_empty() {
                    out.push(prefix.trim_end_matches('.').to_string());
                }
            }
            _ => out.push(prefix.trim_end_matches('.').to_string()),
        }
    }

    #[test]
    fn every_leaf_of_default_and_full_fixture_is_registered() {
        let mut paths = Vec::new();
        leaf_paths(
            &toml::Value::try_from(DaemonConfig::default()).unwrap(),
            "",
            &mut paths,
        );
        let fixture = toml::from_str::<DaemonConfig>(
            "workspace = \"/tmp/ws\"\nmodel = \"zai/glm-5.3\"\nfallback = [\"x\"]\nmode = \"free\"\ntimezone = \"Europe/Madrid\"\n\
             [budgets]\ndaily_usd = 5.0\ndaily_tokens = 100000\n\
             [ka]\nengine_layer = \"/tmp/ka.toml\"\n\
             [telegram]\nenabled = true\ntoken_secret = \"tg\"\nallow_user_ids = [1]\napi_base = \"http://x\"\nstream = false\n\
             [matrix]\nenabled = true\nhomeserver = \"http://h\"\nuser_id = \"@a:b\"\ntoken_secret = \"mx\"\n\
             [web]\nenabled = true\nport = 9000\n\
             [calendar]\ndigest_time = \"07:30\"\n\
             [search]\nprovider = \"brave\"\nbase_url = \"http://s\"\n\
             [[mcp]]\nname = \"demo\"\nurl = \"http://m\"\n\
             [voice]\nprovider = \"local\"\nbase_url = \"http://v\"\nmodel = \"whisper\"\n\
             [notify]\nquiet_hours = \"22:00-07:00\"\n\
             [approvals]\nauto_review = true\n",
        )
        .unwrap();
        leaf_paths(&toml::Value::try_from(fixture).unwrap(), "", &mut paths);
        for p in paths {
            assert!(lookup(&p).is_some(), "unregistered config key: {p}");
        }
    }

    #[test]
    fn scalar_defaults_match_the_serialized_default() {
        let def = toml::Value::try_from(DaemonConfig::default()).unwrap();
        for k in KEYS.iter().filter(|k| k.kind != Kind::Table) {
            let actual = value_at(&def, k.path);
            match actual {
                None => assert!(
                    k.default.contains("unset")
                        || k.default.contains("system")
                        || k.default.contains("search off")
                        || k.default.contains("voice off"),
                    "{}: default is absent but spec says {:?}",
                    k.path,
                    k.default
                ),
                Some(v) => {
                    let rendered = match v {
                        toml::Value::String(s) => s.clone(),
                        other => other.to_string(),
                    };
                    // numeric equality, not textual (1.0 vs 1); strings
                    // compare exactly except the documented "unset (…)"
                    // placeholders
                    if let (Ok(a), Ok(b)) = (rendered.parse::<f64>(), k.default.parse::<f64>()) {
                        assert_eq!(a, b, "{}: default {:?}", k.path, k.default);
                    } else if k.default != "unset" && !k.default.contains('(') {
                        assert_eq!(rendered, k.default, "{}: default {:?}", k.path, k.default);
                    }
                }
            }
        }
    }

    #[test]
    fn lookup_resolves_prefixes_and_suggests_near_misses() {
        assert!(lookup("mcp").is_some());
        assert_eq!(lookup("mcp.0.name").unwrap().path, "mcp");
        assert!(lookup("nope").is_none());
        assert_eq!(suggest("budget.daily_usd"), Some("budgets.daily_usd"));
        assert_eq!(suggest("timezone2"), Some("timezone"));
        assert_eq!(suggest("aaaaaaaaaaaaaaaa"), None);
    }

    #[test]
    fn changed_reports_only_differing_keys() {
        let old = DaemonConfig::default();
        let mut new = old.clone();
        new.mode = "free".to_string();
        new.web.port = 9999;
        let changed = changed(&old, &new);
        let paths: Vec<_> = changed.iter().map(|k| k.path).collect();
        assert_eq!(paths, vec!["mode", "web.port"]);
        assert!(changed[0].apply == Apply::Worker);
        assert!(changed[1].apply == Apply::Restart);
    }
}
