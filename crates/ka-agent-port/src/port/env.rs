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
    /// Permission mode re-asserted after attach (`guarded` default).
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
}
