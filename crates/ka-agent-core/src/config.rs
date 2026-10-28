//! `ka-agent.toml` — strict daemon configuration (SEED.md §11.2: unknown
//! keys are hard errors, mirroring ka). Secrets are never in this file: the
//! Telegram token is a name in the secret store.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum Error {
    #[error("read {path}: {source}")]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("parse {path}: {message}")]
    Parse { path: PathBuf, message: String },
    #[error("invalid config: {0}")]
    Validate(String),
    /// A `config_file` edit that cannot be expressed (array/table targets,
    /// write failures).
    #[error("{0}")]
    Edit(String),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct WorkerRestart {
    /// First backoff after a worker crash.
    pub backoff_start_ms: u64,
    /// Backoff ceiling.
    pub backoff_max_ms: u64,
    /// Crash-loop window and threshold: N crashes inside the window degrade
    /// the session.
    pub crash_window_secs: u64,
    pub crash_max: u32,
}

impl Default for WorkerRestart {
    fn default() -> Self {
        Self {
            backoff_start_ms: 1_000,
            backoff_max_ms: 60_000,
            crash_window_secs: 600,
            crash_max: 5,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Budgets {
    /// Daily USD cap across sessions (ka usage accounting).
    pub daily_usd: Option<f64>,
    /// Daily token cap (input+output+cache); the only cap that binds for
    /// unpriced models.
    pub daily_tokens: Option<u64>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Ka {
    /// Optional ka engine layer (ka's own strict `ka.toml` format), validated
    /// with `Config::parse_layer` by the worker.
    pub engine_layer: Option<PathBuf>,
}

/// One operator hook (`[[hooks]]`, plan 2.7).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct HookSpec {
    /// The event to react to: an agent event (`message_received`,
    /// `turn_finished`, `approval_requested`, `approval_settled`,
    /// `reminder_fired`, `cron_finished`, `session_paused`,
    /// `memory_proposed`) or an engine event (`pre_tool_use`,
    /// `post_tool_use`, `user_prompt_submit`, `session_start`,
    /// `session_end`, `stop`, `turn_end`, `pre_compact`).
    pub on: String,
    /// `sh -c <command>` (agent events) / the engine's own hook contract
    /// (engine events).
    pub command: String,
    /// Only this tool (engine tool-call events only).
    pub tool: Option<String>,
    /// Agent-event timeout (1–60 s; default 10). Engine events use the
    /// engine's own 30 s ceiling.
    pub timeout_secs: Option<u64>,
}

/// The agent-event names a `[[hooks]]` entry may use.
pub const AGENT_HOOK_EVENTS: &[&str] = &[
    "message_received",
    "turn_finished",
    "approval_requested",
    "approval_settled",
    "reminder_fired",
    "cron_finished",
    "session_paused",
    "memory_proposed",
];

/// The engine-event names a `[[hooks]]` entry may use.
pub const ENGINE_HOOK_EVENTS: &[&str] = &[
    "pre_tool_use",
    "post_tool_use",
    "user_prompt_submit",
    "session_start",
    "session_end",
    "stop",
    "turn_end",
    "pre_compact",
];

/// The Matrix channel (Phase 6): strict `[matrix]` section.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct MatrixSettings {
    pub enabled: bool,
    /// Base URL of the homeserver; tests point it at a local fake.
    pub homeserver: Option<String>,
    /// The bot account, `@ka-agent:example.org`.
    pub user_id: String,
    /// Name of the access token in the secret store.
    pub token_secret: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Telegram {
    /// Enable the Telegram channel in the daemon.
    pub enabled: bool,
    /// Name of the bot token in the secret store.
    pub token_secret: Option<String>,
    /// Deprecated: paired operator user ids. Migrated into the identities
    /// table at startup (SEED.md §8.1); link identities with
    /// `ka-agent identity link` instead.
    pub allow_user_ids: Vec<i64>,
    /// Bot API base URL. Default `https://api.telegram.org`; tests point it
    /// at a local fake server.
    pub api_base: Option<String>,
    /// Stream replies as they are written (Bot API `sendMessageDraft`,
    /// private chats). Falls back to the finished message by itself when
    /// the API refuses drafts. Default on.
    pub stream: bool,
}

impl Default for Telegram {
    fn default() -> Self {
        Self {
            enabled: false,
            token_secret: None,
            allow_user_ids: Vec::new(),
            api_base: None,
            stream: true,
        }
    }
}

impl Telegram {
    pub fn api_base(&self) -> &str {
        self.api_base
            .as_deref()
            .unwrap_or("https://api.telegram.org")
    }
}

/// The local web control panel: strict `[web]` section.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Web {
    /// Serve the web UI on `127.0.0.1:{port}`.
    pub enabled: bool,
    pub port: u16,
}

impl Default for Web {
    fn default() -> Self {
        Self {
            enabled: true,
            port: 8480,
        }
    }
}

/// The calendar digest: strict `[calendar]` section.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Calendar {
    /// Local clock time (operator zone) of the daily digest, `HH:MM`.
    pub digest_time: String,
}

impl Default for Calendar {
    fn default() -> Self {
        Self {
            digest_time: "08:00".to_string(),
        }
    }
}

/// Web search for the engine's `web_search` tool: strict `[search]`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Search {
    /// `tavily` | `brave` | `bing`.
    pub provider: String,
    /// API base override (proxies, test fixtures).
    #[serde(default)]
    pub base_url: Option<String>,
}

impl Search {
    pub const PROVIDERS: &'static [&'static str] = &["tavily", "brave", "bing"];

    /// The engine `.env` / environment variable holding this provider's key.
    pub fn key_env(&self) -> String {
        format!("{}_API_KEY", self.provider.to_ascii_uppercase())
    }
}

/// Approval tuning: strict `[approvals]`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Approvals {
    /// Let the engine's fast model auto-allow unmistakably safe commands
    /// (it never denies; anything uncertain still asks you). Off by default.
    pub auto_review: bool,
}

/// Proactive-message manners: strict `[notify]`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Notify {
    /// `"22:00-07:00"`: scheduled prompts (crons, the briefing) that fall
    /// due inside this local window wait until it ends. Reminders and
    /// approvals are never held.
    pub quiet_hours: Option<String>,
}

impl Notify {
    /// The parsed window: (start, end) as (hh, mm).
    pub fn quiet_window(&self) -> Option<((u8, u8), (u8, u8))> {
        let (a, b) = self.quiet_hours.as_deref()?.split_once('-')?;
        Some((crate::when::parse_hhmm(a)?, crate::when::parse_hhmm(b)?))
    }

    /// When `now` falls inside the quiet window: the instant it ends.
    pub fn quiet_until(&self, now: &jiff::Zoned) -> Option<i64> {
        let ((sh, sm), (eh, em)) = self.quiet_window()?;
        let minute = |h: u8, m: u8| u32::from(h) * 60 + u32::from(m);
        let (start, end) = (minute(sh, sm), minute(eh, em));
        let t = minute(now.hour() as u8, now.minute() as u8);
        let inside = if start <= end {
            t >= start && t < end
        } else {
            // Over midnight (22:00-07:00).
            t >= start || t < end
        };
        inside.then(|| crate::when::next_local(eh, em, &crate::when::DayRule::Daily, now))
    }
}

/// Voice-note transcription: strict `[voice]`. OpenAI-compatible
/// `/audio/transcriptions` — OpenAI, Groq, or a local Whisper server.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Voice {
    /// `openai` | `groq` | `local`.
    pub provider: String,
    /// API base (required for `local`, e.g. `http://127.0.0.1:8000/v1`).
    #[serde(default)]
    pub base_url: Option<String>,
    /// Transcription model; a sensible default per provider.
    #[serde(default)]
    pub model: Option<String>,
}

impl Voice {
    pub const PROVIDERS: &'static [&'static str] = &["openai", "groq", "local"];

    /// API base without the trailing `/audio/transcriptions`.
    pub fn base(&self) -> String {
        match (&self.base_url, self.provider.as_str()) {
            (Some(b), _) => b.trim_end_matches('/').to_string(),
            (None, "groq") => "https://api.groq.com/openai/v1".into(),
            (None, _) => "https://api.openai.com/v1".into(),
        }
    }

    pub fn model(&self) -> String {
        match (&self.model, self.provider.as_str()) {
            (Some(m), _) => m.clone(),
            (None, "groq") => "whisper-large-v3-turbo".into(),
            (None, _) => "whisper-1".into(),
        }
    }

    /// The engine `.env` key the provider needs (`None` for a local server).
    pub fn key_env(&self) -> Option<&'static str> {
        match self.provider.as_str() {
            "openai" => Some("OPENAI_API_KEY"),
            "groq" => Some("GROQ_API_KEY"),
            _ => None,
        }
    }
}

/// One operator-declared MCP server (`[[mcp]]`). Values written
/// `secret:<name>` in `env`/`headers` resolve from the secret store when the
/// worker starts, so tokens never live in the config file.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct McpServer {
    /// Tool prefix: tools appear to the model as `<name>.<tool>`.
    pub name: String,
    /// stdio transport: the executable (exclusive with `url`).
    #[serde(default)]
    pub command: Option<String>,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: std::collections::BTreeMap<String, String>,
    /// Streamable-HTTP transport (exclusive with `command`).
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub headers: std::collections::BTreeMap<String, String>,
}

impl McpServer {
    /// `[a-z0-9_-]`, 1..=32, not the built-in bridge's `kagent`.
    pub fn valid_name(name: &str) -> bool {
        (1..=32).contains(&name.len())
            && name != "kagent"
            && name
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-')
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct DaemonConfig {
    /// The single workspace directory every engine runs in (MVP). The
    /// default lives under the default data root — a relative default
    /// would silently create `ka-agent-workspace/` wherever the operator
    /// happened to stand.
    pub workspace: PathBuf,
    /// Default ka model selector for new sessions.
    pub model: Option<String>,
    /// Models tried in order when the main one fails (outage, rate
    /// limit); at most two hops per turn.
    pub fallback: Vec<String>,
    /// Reasoning effort sent to the engine (`off|low|medium|high|max`);
    /// `None` keeps the model's own default.
    pub effort: Option<String>,
    /// Model for background turns (crons, the briefing, `--background`
    /// asks and `-bg` sources); `None` uses the main model.
    pub background_model: Option<String>,
    /// OS sandbox for bash: `auto` (default: fs when a backend exists) |
    /// `fs` | `off`.
    pub sandbox: String,
    /// A message that arrives mid-turn: `steer` (default, rides the
    /// running reply) or `queue` (durable, next turn).
    pub busy_messages: String,
    /// `ask` (default) | `accept_edits` | `allow` | `readonly`. The old
    /// names (`guarded`, `free`, `plan`, `accept-edits`) are accepted
    /// forever and normalized at load ([`normalize_mode`]).
    pub mode: String,
    /// Approval card TTL; deny on expiry.
    pub approval_ttl_secs: u64,
    /// Grace after `/stop` before SIGTERM/SIGKILL escalation.
    pub turn_grace_secs: u64,
    /// Stop an engine worker after this many idle seconds (no live turn,
    /// no pending ask) to release its process; the next message restarts
    /// it and resumes the strand. `0` keeps the worker forever. A resident
    /// daemon that answers once a day should not pay 24/7 for it.
    pub worker_idle_timeout_secs: u64,
    #[serde(rename = "worker_restart")]
    pub worker_restart: WorkerRestart,
    pub budgets: Budgets,
    pub ka: Ka,
    pub telegram: Telegram,
    /// The Matrix channel (Phase 6).
    #[serde(default)]
    pub matrix: MatrixSettings,
    /// Operator timezone (IANA name, e.g. `Europe/Madrid`). `None` uses the
    /// system zone (UTC when it cannot be determined).
    pub timezone: Option<String>,
    pub web: Web,
    pub calendar: Calendar,
    /// Web search for the engine (`None` = the `web_search` tool is off).
    pub search: Option<Search>,
    /// Operator-declared MCP servers.
    pub mcp: Vec<McpServer>,
    /// Voice-note transcription (`None` = voice notes get a setup hint).
    pub voice: Option<Voice>,
    pub notify: Notify,
    pub approvals: Approvals,
    /// Operator hooks ([[hooks]]).
    #[serde(default)]
    pub hooks: Vec<HookSpec>,
}

impl Default for DaemonConfig {
    fn default() -> Self {
        Self {
            workspace: PathBuf::from("~/.ka-agent/profiles/default/workspace"),
            model: None,
            fallback: Vec::new(),
            effort: None,
            background_model: None,
            sandbox: "auto".to_string(),
            busy_messages: "steer".to_string(),
            mode: "ask".to_string(),
            approval_ttl_secs: 900,
            turn_grace_secs: 10,
            worker_idle_timeout_secs: 900,
            worker_restart: WorkerRestart::default(),
            budgets: Budgets::default(),
            ka: Ka::default(),
            telegram: Telegram::default(),
            matrix: MatrixSettings::default(),
            timezone: None,
            web: Web::default(),
            calendar: Calendar::default(),
            search: None,
            mcp: Vec::new(),
            voice: None,
            notify: Notify::default(),
            approvals: Approvals::default(),
            hooks: Vec::new(),
        }
    }
}

/// The canonical permission-mode word for an operator's input, or `None`
/// when it names no mode. The pre-rename vocabulary is accepted forever:
/// `guarded` → `ask`, `free` → `allow`, `plan` → `readonly`,
/// `accept-edits` → `accept_edits` (`accept_edits` kept its name).
pub fn normalize_mode(s: &str) -> Option<String> {
    Some(
        match s {
            "ask" | "guarded" => "ask",
            "accept_edits" | "accept-edits" => "accept_edits",
            "allow" | "free" => "allow",
            "readonly" | "plan" => "readonly",
            _ => return None,
        }
        .to_string(),
    )
}

/// The validation error for a mode word that names nothing (neither the
/// canonical vocabulary nor one of the old aliases).
fn unknown_mode(mode: &str) -> String {
    format!(
        "unknown mode {mode:?} (ask | accept_edits | allow | readonly; \
         the old names guarded/accept_edits/free/plan still work)"
    )
}

impl DaemonConfig {
    /// Load and validate. `~` expands against `$HOME`.
    pub fn load(path: &Path) -> Result<Self, Error> {
        let text = std::fs::read_to_string(path).map_err(|source| Error::Io {
            path: path.to_path_buf(),
            source,
        })?;
        let cfg: Self = toml::from_str(&text).map_err(|e| Error::Parse {
            path: path.to_path_buf(),
            message: e.to_string(),
        })?;
        // A config that pins nothing gets the ACTIVE profile's workspace —
        // profiles must never share one by accident of the default value.
        let has_workspace = toml::from_str::<toml::Value>(&text)
            .ok()
            .and_then(|v| v.get("workspace").cloned())
            .is_some();
        let mut cfg = cfg;
        cfg.set_default_workspace(has_workspace, &crate::layout::default_root());
        // Old-vocabulary modes normalize here: everything downstream
        // (badges, `/mode` replies, the worker's mode word) speaks only
        // `ask | accept_edits | allow | readonly`.
        cfg.mode =
            normalize_mode(&cfg.mode).ok_or_else(|| Error::Validate(unknown_mode(&cfg.mode)))?;
        cfg.validate()?;
        Ok(cfg)
    }

    pub fn validate(&self) -> Result<(), Error> {
        if normalize_mode(&self.mode).is_none() {
            return Err(Error::Validate(unknown_mode(&self.mode)));
        }
        for h in &self.hooks {
            if !AGENT_HOOK_EVENTS.contains(&h.on.as_str())
                && !ENGINE_HOOK_EVENTS.contains(&h.on.as_str())
            {
                let near = AGENT_HOOK_EVENTS
                    .iter()
                    .chain(ENGINE_HOOK_EVENTS)
                    .min_by_key(|e| e.chars().zip(h.on.chars()).filter(|(a, b)| a != b).count());
                return Err(Error::Validate(format!(
                    "unknown hook event {:?} (did you mean {:?}?)",
                    h.on,
                    near.unwrap_or(&"turn_finished")
                )));
            }
            if h.command.trim().is_empty() {
                return Err(Error::Validate(format!(
                    "hook {:?}: command must not be empty",
                    h.on
                )));
            }
            if h.on != "pre_tool_use" && h.on != "post_tool_use" && h.tool.is_some() {
                return Err(Error::Validate(format!(
                    "hook {:?}: `tool` only applies to pre_tool_use/post_tool_use",
                    h.on
                )));
            }
            if let Some(t) = h.timeout_secs
                && !(1..=60).contains(&t)
            {
                return Err(Error::Validate(format!(
                    "hook {:?}: timeout_secs must be in 1..=60",
                    h.on
                )));
            }
        }
        if let Some(effort) = &self.effort
            && !matches!(effort.as_str(), "off" | "low" | "medium" | "high" | "max")
        {
            return Err(Error::Validate(format!(
                "unknown effort {effort:?} (off|low|medium|high|max)"
            )));
        }
        if !matches!(self.sandbox.as_str(), "auto" | "fs" | "off") {
            return Err(Error::Validate(format!(
                "unknown sandbox {:?} (auto|fs|off)",
                self.sandbox
            )));
        }
        if !matches!(self.busy_messages.as_str(), "steer" | "queue") {
            return Err(Error::Validate(format!(
                "unknown busy_messages {:?} (steer|queue)",
                self.busy_messages
            )));
        }
        if self.approval_ttl_secs == 0 || self.approval_ttl_secs > 86_400 {
            return Err(Error::Validate(
                "approval_ttl_secs must be in 1..=86400".into(),
            ));
        }
        if self.turn_grace_secs == 0 || self.turn_grace_secs > 600 {
            return Err(Error::Validate("turn_grace_secs must be in 1..=600".into()));
        }
        // 0 disables the idle reap; anything set is capped at a week so a
        // typo cannot silently disable it forever.
        if self.worker_idle_timeout_secs > 7 * 86_400 {
            return Err(Error::Validate(
                "worker_idle_timeout_secs must be 0 (off) or at most one week".into(),
            ));
        }
        if self.workspace.as_os_str().is_empty() {
            return Err(Error::Validate("workspace must not be empty".into()));
        }
        // The workspace is what the engine may write freely; ka-agent's own
        // files are guarded by path. A workspace that overlaps them either
        // way round would fight the guard (or starve the engine) — refuse.
        {
            let base = crate::layout::base_dir();
            let data = crate::layout::default_root();
            let protected = [
                data.join("config.toml"),
                data.join("state.db"),
                data.join("secrets"),
                data.join("memory"),
                data.join("engine-home"),
                data.join("engine-state"),
                data.join("ka"),
                data.join("backups"),
                data.join("logs"),
                base.join("config.toml"),
                base.join("active_profile"),
                base.join("skills"),
            ];
            let ws = self.workspace_path();
            // canonical when it exists; lexical fallback otherwise
            let ws = std::fs::canonicalize(&ws).unwrap_or(ws);
            for p in &protected {
                let p = std::fs::canonicalize(p).unwrap_or_else(|_| p.clone());
                if ws == p || ws.starts_with(&p) || p.starts_with(&ws) {
                    return Err(Error::Validate(format!(
                        "workspace {} overlaps ka-agent's own files ({}) — pick a directory outside the data and profile roots",
                        ws.display(),
                        p.display()
                    )));
                }
            }
        }
        if let Some(usd) = self.budgets.daily_usd
            && usd <= 0.0
        {
            return Err(Error::Validate("budgets.daily_usd must be positive".into()));
        }
        if self.telegram.enabled && self.telegram.token_secret.is_none() {
            return Err(Error::Validate(
                "telegram.enabled requires telegram.token_secret (a secret-store name)".into(),
            ));
        }
        if self.matrix.enabled {
            if self.matrix.homeserver.is_none() {
                return Err(Error::Validate(
                    "matrix.enabled requires matrix.homeserver".into(),
                ));
            }
            if self.matrix.user_id.is_empty() || !self.matrix.user_id.starts_with('@') {
                return Err(Error::Validate(
                    "matrix.enabled requires matrix.user_id (@user:domain)".into(),
                ));
            }
            if self.matrix.token_secret.is_none() {
                return Err(Error::Validate(
                    "matrix.enabled requires matrix.token_secret (a secret-store name)".into(),
                ));
            }
        }
        if let Some(name) = &self.timezone
            && jiff::tz::TimeZone::get(name).is_err()
        {
            return Err(Error::Validate(format!(
                "timezone {name:?} is not a known IANA zone (e.g. \"Europe/Madrid\")"
            )));
        }
        if self.web.port == 0 {
            return Err(Error::Validate("web.port must not be 0".into()));
        }
        if crate::when::parse_hhmm(&self.calendar.digest_time).is_none() {
            return Err(Error::Validate(
                "calendar.digest_time must be HH:MM (24h)".into(),
            ));
        }
        if self.fallback.len() > 2 {
            return Err(Error::Validate(
                "fallback takes at most 2 models (the engine tries 2 hops per turn)".into(),
            ));
        }
        if let Some(search) = &self.search
            && !Search::PROVIDERS.contains(&search.provider.as_str())
        {
            return Err(Error::Validate(format!(
                "search.provider {:?}: tavily | brave | bing",
                search.provider
            )));
        }
        if self.notify.quiet_hours.is_some() && self.notify.quiet_window().is_none() {
            return Err(Error::Validate(
                "notify.quiet_hours must be \"HH:MM-HH:MM\" (e.g. \"22:00-07:00\")".into(),
            ));
        }
        if let Some(v) = &self.voice {
            if !Voice::PROVIDERS.contains(&v.provider.as_str()) {
                return Err(Error::Validate(format!(
                    "voice.provider {:?}: openai | groq | local",
                    v.provider
                )));
            }
            if v.provider == "local" && v.base_url.is_none() {
                return Err(Error::Validate(
                    "voice.provider = \"local\" needs voice.base_url (your Whisper server)".into(),
                ));
            }
        }
        let mut seen = std::collections::BTreeSet::new();
        for m in &self.mcp {
            if !McpServer::valid_name(&m.name) {
                return Err(Error::Validate(format!(
                    "mcp name {:?}: [a-z0-9_-], up to 32 chars, not \"kagent\"",
                    m.name
                )));
            }
            if !seen.insert(m.name.as_str()) {
                return Err(Error::Validate(format!(
                    "mcp name {:?} is used twice",
                    m.name
                )));
            }
            if m.command.is_some() == m.url.is_some() {
                return Err(Error::Validate(format!(
                    "mcp {:?} needs exactly one of `command` (stdio) or `url` (http)",
                    m.name
                )));
            }
        }
        Ok(())
    }

    /// The operator's zone: the configured IANA zone, else the system zone
    /// (jiff falls back to UTC when the system zone is unknown).
    pub fn time_zone(&self) -> jiff::tz::TimeZone {
        self.timezone
            .as_deref()
            .and_then(|name| jiff::tz::TimeZone::get(name).ok())
            .unwrap_or_else(jiff::tz::TimeZone::system)
    }

    /// When the file pins no `workspace`, resolve it inside `root` (the
    /// active data root) instead of the hardcoded default-profile path.
    fn set_default_workspace(&mut self, has_key: bool, root: &Path) {
        if !has_key {
            self.workspace = root.join("workspace");
        }
    }

    /// Workspace path with `~` expanded.
    pub fn workspace_path(&self) -> PathBuf {
        crate::expand_home(&self.workspace.display().to_string())
    }

    /// Engine layer path with `~` expanded.
    pub fn engine_layer_path(&self) -> Option<PathBuf> {
        self.ka
            .engine_layer
            .as_ref()
            .map(|p| crate::expand_home(&p.display().to_string()))
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    fn write_tmp(name: &str, content: &str) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("ka-agent-core-cfg-{}-{name}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("ka-agent.toml");
        std::fs::write(&p, content).unwrap();
        p
    }

    #[test]
    fn workspace_default_follows_the_active_root_when_absent() {
        let mut cfg = DaemonConfig::default();
        cfg.set_default_workspace(false, std::path::Path::new("/data/pinned"));
        assert_eq!(
            cfg.workspace_path(),
            std::path::PathBuf::from("/data/pinned/workspace")
        );
        // An explicit key is untouched.
        let mut explicit = DaemonConfig {
            workspace: std::path::PathBuf::from("/explicit/ws"),
            ..DaemonConfig::default()
        };
        explicit.set_default_workspace(true, std::path::Path::new("/data/pinned"));
        assert_eq!(
            explicit.workspace_path(),
            std::path::PathBuf::from("/explicit/ws")
        );
    }

    #[test]
    fn minimal_config_loads_with_defaults() {
        let p = write_tmp("minimal", "workspace = \"/tmp/ws\"\n");
        let cfg = DaemonConfig::load(&p).unwrap();
        assert_eq!(cfg.mode, "ask");
        assert_eq!(cfg.approval_ttl_secs, 900);
        assert!(cfg.model.is_none());
    }

    #[test]
    fn unknown_keys_are_hard_errors() {
        let p = write_tmp(
            "unknown",
            "workspace = \"/tmp/ws\"\ntelegram-token = \"x\"\n",
        );
        let err = DaemonConfig::load(&p).unwrap_err();
        assert!(err.to_string().contains("parse"), "{err}");
    }

    #[test]
    fn unknown_mode_rejected() {
        let cfg = DaemonConfig {
            mode: "yolo".into(),
            ..DaemonConfig::default()
        };
        let err = cfg.validate().unwrap_err().to_string();
        assert!(
            err.contains("ask | accept_edits | allow | readonly"),
            "{err}"
        );
        assert!(
            err.contains("guarded/accept_edits/free/plan still work"),
            "{err}"
        );
    }

    #[test]
    fn normalize_mode_maps_old_names_and_keeps_canonical() {
        // Canonical words pass through untouched.
        for word in ["ask", "accept_edits", "allow", "readonly"] {
            assert_eq!(normalize_mode(word).as_deref(), Some(word));
        }
        // The old vocabulary is accepted forever as an alias.
        assert_eq!(normalize_mode("guarded").as_deref(), Some("ask"));
        assert_eq!(normalize_mode("free").as_deref(), Some("allow"));
        assert_eq!(normalize_mode("plan").as_deref(), Some("readonly"));
        assert_eq!(
            normalize_mode("accept-edits").as_deref(),
            Some("accept_edits")
        );
        // Anything else names no mode.
        assert_eq!(normalize_mode("yolo"), None);
        assert_eq!(normalize_mode(""), None);
    }

    #[test]
    fn old_mode_names_normalize_at_load() {
        let p = write_tmp("alias-ask", "workspace = \"/tmp/ws\"\nmode = \"guarded\"\n");
        assert_eq!(DaemonConfig::load(&p).unwrap().mode, "ask");
        let p = write_tmp("alias-allow", "workspace = \"/tmp/ws\"\nmode = \"free\"\n");
        assert_eq!(DaemonConfig::load(&p).unwrap().mode, "allow");
        let p = write_tmp(
            "alias-readonly",
            "workspace = \"/tmp/ws\"\nmode = \"plan\"\n",
        );
        assert_eq!(DaemonConfig::load(&p).unwrap().mode, "readonly");
        let p = write_tmp(
            "alias-kebab",
            "workspace = \"/tmp/ws\"\nmode = \"accept-edits\"\n",
        );
        assert_eq!(DaemonConfig::load(&p).unwrap().mode, "accept_edits");
    }

    #[test]
    fn quiet_hours_span_midnight_and_end_at_the_window_end() {
        let n = Notify {
            quiet_hours: Some("22:00-07:00".into()),
        };
        let at = |s: &str| -> jiff::Zoned { s.parse().unwrap() };
        let night = at("2026-10-29T23:30:00+01:00[Europe/Madrid]");
        let end = n.quiet_until(&night).expect("inside");
        let end = jiff::Timestamp::from_millisecond(end)
            .unwrap()
            .to_zoned(night.time_zone().clone());
        assert_eq!((end.day(), end.hour(), end.minute()), (30, 7, 0));
        assert!(
            n.quiet_until(&at("2026-10-30T06:59:00+01:00[Europe/Madrid]"))
                .is_some()
        );
        assert!(
            n.quiet_until(&at("2026-10-30T07:00:00+01:00[Europe/Madrid]"))
                .is_none()
        );
        assert!(
            n.quiet_until(&at("2026-10-30T12:00:00+01:00[Europe/Madrid]"))
                .is_none()
        );
        let day = Notify {
            quiet_hours: Some("13:00-15:00".into()),
        };
        assert!(
            day.quiet_until(&at("2026-10-30T14:00:00+01:00[Europe/Madrid]"))
                .is_some()
        );
        let cfg = DaemonConfig {
            notify: Notify {
                quiet_hours: Some("late-early".into()),
            },
            ..DaemonConfig::default()
        };
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn telegram_no_longer_requires_allow_user_ids() {
        // Enabled with no allow_user_ids is valid: identities live in the
        // store now and the config list is only a migration source.
        let cfg = DaemonConfig {
            telegram: Telegram {
                enabled: true,
                token_secret: Some("telegram.bot_token".into()),
                ..Telegram::default()
            },
            ..DaemonConfig::default()
        };
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn full_shape_roundtrips() {
        let toml_src = r#"
workspace = "~/ws"
model = "zai/glm-5.3-flash"
mode = "plan"
approval_ttl_secs = 300
turn_grace_secs = 5
timezone = "Europe/Madrid"

[worker_restart]
backoff_start_ms = 500
backoff_max_ms = 30_000
crash_window_secs = 300
crash_max = 3

[budgets]
daily_usd = 5.0
daily_tokens = 1_000_000

[ka]
engine_layer = "~/engine.toml"

[telegram]
enabled = false
token_secret = "telegram.bot_token"
allow_user_ids = [12345]

[web]
enabled = false
port = 9000

[calendar]
digest_time = "07:30"
"#;
        let p = write_tmp("full", toml_src);
        let cfg = DaemonConfig::load(&p).unwrap();
        // `plan` is the old name; load answers with the canonical word.
        assert_eq!(cfg.mode, "readonly");
        assert_eq!(cfg.worker_restart.crash_max, 3);
        assert_eq!(cfg.budgets.daily_usd, Some(5.0));
        assert_eq!(cfg.telegram.allow_user_ids, vec![12345]);
        assert!(cfg.workspace_path().ends_with("ws"));
        assert_eq!(cfg.timezone.as_deref(), Some("Europe/Madrid"));
        assert_eq!(cfg.time_zone().iana_name(), Some("Europe/Madrid"));
        assert!(!cfg.web.enabled);
        assert_eq!(cfg.web.port, 9000);
        assert_eq!(cfg.calendar.digest_time, "07:30");
    }

    #[test]
    fn web_and_calendar_defaults() {
        let cfg = DaemonConfig::default();
        assert!(cfg.web.enabled);
        assert_eq!(cfg.web.port, 8480);
        assert_eq!(cfg.calendar.digest_time, "08:00");
        assert!(cfg.timezone.is_none());
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn bad_timezone_rejected() {
        let cfg = DaemonConfig {
            timezone: Some("Mars/Olympus".into()),
            ..DaemonConfig::default()
        };
        let err = cfg.validate().unwrap_err().to_string();
        assert_eq!(
            err,
            "invalid config: timezone \"Mars/Olympus\" is not a known IANA zone (e.g. \"Europe/Madrid\")"
        );
    }

    #[test]
    fn web_port_zero_rejected() {
        let mut cfg = DaemonConfig::default();
        cfg.web.port = 0;
        assert!(cfg.validate().unwrap_err().to_string().contains("web.port"));
    }

    #[test]
    fn bad_digest_time_rejected() {
        let mut cfg = DaemonConfig::default();
        cfg.calendar.digest_time = "25:00".into();
        let err = cfg.validate().unwrap_err().to_string();
        assert!(
            err.contains("calendar.digest_time must be HH:MM (24h)"),
            "{err}"
        );
    }

    #[test]
    fn default_workspace_is_absolute_under_the_data_root() {
        // A relative default would create the workspace wherever the
        // operator happened to stand (`ka-agent-workspace/` litter).
        let ws = DaemonConfig::default().workspace_path();
        assert!(
            ws.is_absolute(),
            "default workspace must be absolute: {ws:?}"
        );
        assert!(
            ws.ends_with(".ka-agent/profiles/default/workspace"),
            "default workspace lives under the default profile root: {ws:?}"
        );
    }
}
