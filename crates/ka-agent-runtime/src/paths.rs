//! Instance directory layout (SEED.md §4.2). Everything Ka Agent owns lives
//! under one root, overridable with `KA_AGENT_DATA_DIR` (tests, parallel
//! instances) exactly like ka's own `KA_DATA_DIR`.

use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct Paths {
    root: PathBuf,
}

impl Paths {
    /// `$KA_AGENT_DATA_DIR` or `~/.local/share/ka-agent`.
    pub fn default_root() -> PathBuf {
        if let Some(dir) = std::env::var_os("KA_AGENT_DATA_DIR") {
            if !dir.is_empty() {
                return PathBuf::from(dir);
            }
        }
        let base = std::env::var_os("XDG_DATA_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                let home = std::env::var_os("HOME").unwrap_or_default();
                Path::new(&home).join(".local/share")
            });
        base.join("ka-agent")
    }

    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn state_db(&self) -> PathBuf {
        self.root.join("state.db")
    }

    /// KA_DATA_DIR for workers: their strands, waypoints, index.
    pub fn ka_data(&self) -> PathBuf {
        self.root.join("ka")
    }

    /// Synthetic HOME for workers: user-scope skills + compiled MEMORY.md,
    /// resolved via `$HOME/.config/ka/...` by ka's convention discovery.
    pub fn engine_home(&self) -> PathBuf {
        self.root.join("engine-home")
    }

    /// XDG_STATE_HOME for workers: spills, trust store, detached-job table.
    pub fn engine_state(&self) -> PathBuf {
        self.root.join("engine-state")
    }

    pub fn engine_cache(&self) -> PathBuf {
        self.engine_home().join(".cache")
    }

    pub fn attachments(&self) -> PathBuf {
        self.root.join("attachments")
    }

    pub fn backups(&self) -> PathBuf {
        self.root.join("backups")
    }

    pub fn logs(&self) -> PathBuf {
        self.root.join("logs")
    }

    /// Create every directory the daemon and workers need.
    pub fn ensure_dirs(&self) -> std::io::Result<()> {
        for dir in [
            self.root.clone(),
            self.ka_data(),
            self.engine_home().join(".config/ka"),
            self.engine_state(),
            self.engine_cache(),
            self.attachments(),
            self.backups(),
            self.logs(),
        ] {
            std::fs::create_dir_all(dir)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[test]
    fn layout_is_nested_under_root() {
        let root = PathBuf::from("/tmp/ka-agent-test");
        let p = Paths::new(root.clone());
        assert_eq!(p.state_db(), root.join("state.db"));
        assert_eq!(p.ka_data(), root.join("ka"));
        assert_eq!(p.engine_home(), root.join("engine-home"));
        assert_eq!(p.engine_state(), root.join("engine-state"));
        assert_eq!(p.engine_cache(), root.join("engine-home/.cache"));
    }

    #[test]
    fn ensure_dirs_creates_the_tree() {
        let root = std::env::temp_dir().join(format!("ka-agent-paths-{}", std::process::id()));
        let p = Paths::new(root.clone());
        p.ensure_dirs().unwrap();
        assert!(p.ka_data().is_dir());
        assert!(p.engine_home().join(".config/ka").is_dir());
        assert!(p.engine_state().is_dir());
        std::fs::remove_dir_all(root).ok();
    }
}
