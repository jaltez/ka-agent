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

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
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
    /// `guarded` (default) | `accept_edits` | `free` | `plan`.
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
}

impl Default for DaemonConfig {
    fn default() -> Self {
        Self {
            workspace: PathBuf::from("~/.local/share/ka-agent/workspace"),
            model: None,
            mode: "guarded".to_string(),
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
        }
    }
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
        cfg.validate()?;
        Ok(cfg)
    }

    pub fn validate(&self) -> Result<(), Error> {
        if !matches!(
            self.mode.as_str(),
            "guarded" | "accept_edits" | "free" | "plan"
        ) {
            return Err(Error::Validate(format!("unknown mode {:?}", self.mode)));
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
    fn minimal_config_loads_with_defaults() {
        let p = write_tmp("minimal", "workspace = \"/tmp/ws\"\n");
        let cfg = DaemonConfig::load(&p).unwrap();
        assert_eq!(cfg.mode, "guarded");
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
        assert_eq!(cfg.mode, "plan");
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
            ws.ends_with("ka-agent/workspace"),
            "default workspace lives under the data root: {ws:?}"
        );
    }
}
