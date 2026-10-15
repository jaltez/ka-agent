//! `ka-agent.toml` — strict daemon configuration (SEED.md §11.2: unknown
//! keys are hard errors, mirroring ka). Secrets are never in this file: the
//! Telegram token is a name in the secret store.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum Error {
    #[error("read {path}: {source}")]
    Io { path: PathBuf, source: std::io::Error },
    #[error("parse {path}: {message}")]
    Parse { path: PathBuf, message: String },
    #[error("invalid config: {0}")]
    Validate(String),
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
        Self { backoff_start_ms: 1_000, backoff_max_ms: 60_000, crash_window_secs: 600, crash_max: 5 }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Budgets {
    /// Daily USD cap across sessions (ka usage accounting).
    pub daily_usd: Option<f64>,
    /// Daily token cap (input+output+cache); the only cap that binds for
    /// unpriced models.
    pub daily_tokens: Option<u64>,
}

impl Default for Budgets {
    fn default() -> Self {
        Self { daily_usd: None, daily_tokens: None }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Ka {
    /// Optional ka engine layer (ka's own strict `ka.toml` format), validated
    /// with `Config::parse_layer` by the worker.
    pub engine_layer: Option<PathBuf>,
}

impl Default for Ka {
    fn default() -> Self {
        Self { engine_layer: None }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Telegram {
    /// Telegram arrives in the next milestone; the shape is fixed now so the
    /// config schema does not churn.
    pub enabled: bool,
    /// Name of the bot token in the secret store.
    pub token_secret: Option<String>,
    /// Paired operator user ids (numeric; usernames are never trusted).
    pub allow_user_ids: Vec<i64>,
}

impl Default for Telegram {
    fn default() -> Self {
        Self { enabled: false, token_secret: None, allow_user_ids: Vec::new() }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct DaemonConfig {
    /// The single workspace directory every engine runs in (MVP).
    pub workspace: PathBuf,
    /// Default ka model selector for new sessions.
    pub model: Option<String>,
    /// `guarded` (default) | `accept_edits` | `free` | `plan`.
    pub mode: String,
    /// Approval card TTL; deny on expiry.
    pub approval_ttl_secs: u64,
    /// Grace after `/stop` before SIGTERM/SIGKILL escalation.
    pub turn_grace_secs: u64,
    #[serde(rename = "worker_restart")]
    pub worker_restart: WorkerRestart,
    pub budgets: Budgets,
    pub ka: Ka,
    pub telegram: Telegram,
}

impl Default for DaemonConfig {
    fn default() -> Self {
        Self {
            workspace: PathBuf::from("ka-agent-workspace"),
            model: None,
            mode: "guarded".to_string(),
            approval_ttl_secs: 900,
            turn_grace_secs: 10,
            worker_restart: WorkerRestart::default(),
            budgets: Budgets::default(),
            ka: Ka::default(),
            telegram: Telegram::default(),
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
            return Err(Error::Validate("approval_ttl_secs must be in 1..=86400".into()));
        }
        if self.turn_grace_secs == 0 || self.turn_grace_secs > 600 {
            return Err(Error::Validate("turn_grace_secs must be in 1..=600".into()));
        }
        if self.workspace.as_os_str().is_empty() {
            return Err(Error::Validate("workspace must not be empty".into()));
        }
        if let Some(usd) = self.budgets.daily_usd {
            if usd <= 0.0 {
                return Err(Error::Validate("budgets.daily_usd must be positive".into()));
            }
        }
        if self.telegram.enabled && self.telegram.allow_user_ids.is_empty() {
            return Err(Error::Validate(
                "telegram.enabled requires at least one allow_user_ids entry".into(),
            ));
        }
        Ok(())
    }

    /// Workspace path with `~` expanded.
    pub fn workspace_path(&self) -> PathBuf {
        crate::expand_home(&self.workspace.display().to_string())
    }

    /// Engine layer path with `~` expanded.
    pub fn engine_layer_path(&self) -> Option<PathBuf> {
        self.ka.engine_layer
            .as_ref()
            .map(|p| crate::expand_home(&p.display().to_string()))
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    fn write_tmp(name: &str, content: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("ka-agent-core-cfg-{}-{name}", std::process::id()));
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
        let p = write_tmp("unknown", "workspace = \"/tmp/ws\"\ntelegram-token = \"x\"\n");
        let err = DaemonConfig::load(&p).unwrap_err();
        assert!(err.to_string().contains("parse"), "{err}");
    }

    #[test]
    fn unknown_mode_rejected() {
        let cfg = DaemonConfig { mode: "yolo".into(), ..DaemonConfig::default() };
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn telegram_enabled_needs_allowlist() {
        let cfg = DaemonConfig {
            telegram: Telegram { enabled: true, ..Telegram::default() },
            ..DaemonConfig::default()
        };
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn full_shape_roundtrips() {
        let toml_src = r#"
workspace = "~/ws"
model = "zai/glm-5.3-flash"
mode = "plan"
approval_ttl_secs = 300
turn_grace_secs = 5

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
"#;
        let p = write_tmp("full", toml_src);
        let cfg = DaemonConfig::load(&p).unwrap();
        assert_eq!(cfg.mode, "plan");
        assert_eq!(cfg.worker_restart.crash_max, 3);
        assert_eq!(cfg.budgets.daily_usd, Some(5.0));
        assert_eq!(cfg.telegram.allow_user_ids, vec![12345]);
        assert!(cfg.workspace_path().ends_with("ws"));
    }
}
