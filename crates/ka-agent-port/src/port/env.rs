//! Worker environment pinning and the open request. Split from port.rs
//! (moves only).

use std::path::PathBuf;

use tokio::process::Command;

/// Which strand an open attaches to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StrandSelector {
    /// Create a fresh strand.
    New,
    /// Resume the strand at this file path.
    Path(PathBuf),
}

/// The worker's pinned environment (SEED §3.1.5 / §6.4, validation §7):
/// the worker must never inherit the operator's interactive Ka state or
/// ambient credentials, or it silently uses the operator's model, mode
/// and keys.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WorkerEnv {
    /// `HOME` for the worker (compiled `MEMORY.md`, skills).
    pub home: PathBuf,
    /// `KA_DATA_DIR` (strands, waypoints).
    pub data_dir: PathBuf,
    /// `XDG_STATE_HOME` (spills, trust store, jobs).
    pub state_dir: PathBuf,
    /// `XDG_CACHE_HOME` (feeds the sandbox allowlist).
    pub cache_dir: PathBuf,
    /// `KA_AGENT_DATA_DIR` for the worker and its spawned MCP bridge.
    pub agent_data_root: PathBuf,
    /// Paths the engine's file tools may never write (`__guard` list).
    pub guard_write: Vec<PathBuf>,
    /// Paths the engine's file tools may never read (`__guard` list).
    pub guard_read: Vec<PathBuf>,
    /// Grant decisions the `__guard` hook enforces or pre-approves.
    pub guard_grants: GuardGrants,
}

/// What the `__guard` hook may pre-approve or deny beyond the protected
/// lists, serialized as `KA_AGENT_GUARD_GRANTS`. Absent env = all-false:
/// the hook's grant logic stays off and behavior is unchanged.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GuardGrants {
    /// `grants.install` (implicitly true under `full`): install-class
    /// commands are pre-approved instead of denied.
    pub install: bool,
    /// `grants.full`: deny dirs and the install gate are skipped,
    /// bash and file writes are pre-approved. Self-protection stays.
    pub full: bool,
    /// `[[grants.path]]` deny dirs: blocked for bash and file tools.
    pub deny: Vec<PathBuf>,
    /// `[[grants.path]]` allow dirs: `write`/`edit` inside them are
    /// pre-approved (N dirs behave identically — no first-rule quirk).
    pub allow: Vec<PathBuf>,
    /// No operator gate remains for bash (`mode = "allow"` or `full`),
    /// so the whole data root is protected, not just the file list.
    pub hard: bool,
}

impl GuardGrants {
    /// The JSON the hook reads. Keep in sync with `__guard`'s parser.
    fn to_env_json(&self) -> String {
        serde_json::json!({
            "install": self.install,
            "full": self.full,
            "hard": self.hard,
            "deny": self.deny.iter().map(|d| d.to_string_lossy()).collect::<Vec<_>>(),
            "allow": self.allow.iter().map(|d| d.to_string_lossy()).collect::<Vec<_>>(),
        })
        .to_string()
    }
}

/// Env-var name markers whose values are ambient operator credentials —
/// never allowed into a worker. Provider secrets are injected per
/// session by the runtime, not inherited.
const SECRET_MARKERS: &[&str] = &["KEY", "TOKEN", "SECRET", "PASSWORD", "AUTH"];

fn is_secret_name(name: &str) -> bool {
    let upper = name.to_ascii_uppercase();
    SECRET_MARKERS.iter().any(|m| upper.contains(m))
}

impl WorkerEnv {
    /// Prepare a worker [`Command`]'s environment: keep the ambient
    /// variables except credential-shaped ones (matched
    /// case-insensitively on the name), then pin the five
    /// isolation-critical variables. `XDG_CONFIG_HOME` is pinned because
    /// ka's user config layer resolves through it, bypassing `HOME`
    /// (validation finding F-2).
    pub fn sanitize(&self, cmd: &mut Command) {
        let keep: Vec<(std::ffi::OsString, std::ffi::OsString)> = std::env::vars_os()
            .filter(|(name, _)| !name.to_str().is_some_and(is_secret_name))
            .collect();
        cmd.env_clear().envs(keep);
        cmd.env("HOME", &self.home)
            .env("XDG_CONFIG_HOME", self.home.join(".config"))
            .env("XDG_STATE_HOME", &self.state_dir)
            .env("XDG_CACHE_HOME", &self.cache_dir)
            .env("KA_DATA_DIR", &self.data_dir)
            .env("KA_AGENT_DATA_DIR", &self.agent_data_root);
        if !self.guard_write.is_empty()
            && let Ok(joined) = std::env::join_paths(&self.guard_write)
        {
            cmd.env("KA_AGENT_GUARD_WRITE", joined);
        }
        if !self.guard_read.is_empty()
            && let Ok(joined) = std::env::join_paths(&self.guard_read)
        {
            cmd.env("KA_AGENT_GUARD_READ", joined);
        }
        // Always sent (all-false by default): the hook's parser treats
        // absence as all-false, so older daemons stay compatible.
        cmd.env("KA_AGENT_GUARD_GRANTS", self.guard_grants.to_env_json());
    }
}

/// Everything one open needs.
#[derive(Debug, Clone)]
pub struct OpenRequest {
    /// Working directory (also the strand-storage key).
    pub workspace: PathBuf,
    /// Fresh strand or resume-by-path.
    pub strand: StrandSelector,
    /// Model selector re-asserted after attach.
    pub model: Option<String>,
    /// Permission mode re-asserted after attach (`ask` default).
    pub mode: Option<String>,
    /// Ka engine layer TOML applied below the worker's security overlay.
    pub engine_layer_toml: Option<String>,
    /// Isolation environment for the worker process.
    pub env: WorkerEnv,
    /// Worker binary to spawn.
    pub binary: PathBuf,
    /// Extra argv before the worker protocol flags, e.g. `["__worker"]`
    /// when the binary is a multi-command app.
    pub worker_args: Vec<String>,
    /// Scripted-speaker scenario (tests / e2e; `None` in production).
    pub scenario: Option<PathBuf>,
    /// Exact tool names the approval profile never asks about.
    pub auto_allow: Vec<String>,
}

impl Default for OpenRequest {
    fn default() -> Self {
        Self {
            workspace: PathBuf::new(),
            strand: StrandSelector::New,
            model: None,
            mode: None,
            engine_layer_toml: None,
            env: WorkerEnv::default(),
            binary: PathBuf::new(),
            worker_args: Vec::new(),
            scenario: None,
            auto_allow: Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    #[test]
    fn secret_name_filter_matches_markers_case_insensitively() {
        for name in [
            "ANTHROPIC_API_KEY",
            "Zai_Api_Key",
            "GITHUB_TOKEN",
            "SECRET_CODE",
            "MY_PASSWORD",
            "SSH_AUTH_SOCK",
            "AUTHORIZATION",
        ] {
            assert!(is_secret_name(name), "{name}");
        }
        for name in ["PATH", "HOME", "LANG", "TERM", "KA_DATA_DIR", "MODELS"] {
            assert!(!is_secret_name(name), "{name}");
        }
    }

    #[tokio::test]
    async fn sanitize_pins_isolation_and_strips_credentials() {
        let root = std::env::temp_dir().join(format!("ka-port-sanitize-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("home")).unwrap();
        let env = WorkerEnv {
            home: root.join("home"),
            data_dir: root.join("ka"),
            state_dir: root.join("state"),
            cache_dir: root.join("cache"),
            agent_data_root: root.clone(),
            guard_write: Vec::new(),
            guard_read: Vec::new(),
            guard_grants: GuardGrants::default(),
        };
        let mut cmd = Command::new("env");
        env.sanitize(&mut cmd);
        // ambient credentials visible to the test process must not survive
        let output = cmd.output().await.unwrap().stdout;
        let text = String::from_utf8_lossy(&output);
        assert!(
            text.contains(&format!("HOME={}\n", root.join("home").display())),
            "{text}"
        );
        assert!(
            text.contains(&format!(
                "XDG_CONFIG_HOME={}\n",
                root.join("home/.config").display()
            )),
            "{text}"
        );
        assert!(
            text.contains(&format!(
                "XDG_STATE_HOME={}\n",
                root.join("state").display()
            )),
            "{text}"
        );
        assert!(
            text.contains(&format!(
                "XDG_CACHE_HOME={}\n",
                root.join("cache").display()
            )),
            "{text}"
        );
        assert!(
            text.contains(&format!("KA_DATA_DIR={}\n", root.join("ka").display())),
            "{text}"
        );
        assert!(!text.contains("ANTHROPIC_API_KEY"), "{text}");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn sanitize_carries_guard_grants_json() {
        let root = std::env::temp_dir().join(format!("ka-port-grants-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("home")).unwrap();
        let env = WorkerEnv {
            home: root.join("home"),
            guard_grants: GuardGrants {
                install: true,
                full: false,
                deny: vec![PathBuf::from("/srv/nope")],
                allow: vec![PathBuf::from("/srv/ok")],
                hard: true,
            },
            ..WorkerEnv::default()
        };
        let mut cmd = Command::new("env");
        env.sanitize(&mut cmd);
        let output = cmd.output().await.unwrap().stdout;
        let text = String::from_utf8_lossy(&output);
        let line = text
            .lines()
            .find(|l| l.starts_with("KA_AGENT_GUARD_GRANTS="))
            .unwrap_or_else(|| panic!("no grants env in: {text}"));
        let v: serde_json::Value =
            serde_json::from_str(line.trim_start_matches("KA_AGENT_GUARD_GRANTS=")).unwrap();
        assert_eq!(v["install"], serde_json::json!(true));
        assert_eq!(v["full"], serde_json::json!(false));
        assert_eq!(v["hard"], serde_json::json!(true));
        assert_eq!(v["deny"], serde_json::json!(["/srv/nope"]));
        assert_eq!(v["allow"], serde_json::json!(["/srv/ok"]));
        let _ = std::fs::remove_dir_all(&root);
    }
}
