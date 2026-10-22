//! Instance directory layout (SEED.md §4.2). Everything Ka Agent owns lives
//! under one root: `~/.ka-agent/profiles/<profile>/…` by default (see
//! `ka_agent_core::layout` for the env overrides — `KA_AGENT_DATA_DIR`
//! replaces the root wholesale, exactly like ka's own `KA_DATA_DIR`).

use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct Paths {
    root: PathBuf,
}

impl Paths {
    /// The default root (`ka_agent_core::layout::default_root`).
    pub fn default_root() -> PathBuf {
        ka_agent_core::layout::default_root()
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

    /// The single-instance lock file (daemon or local chat).
    pub fn instance_lock(&self) -> PathBuf {
        self.root.join("instance.lock")
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
        // Memory, state.db, logs and the workspace are personal: only the
        // owner may enter the tree (secrets are 0600 on their own). Applied
        // on every start, so older 0755 installs tighten themselves.
        restrict_dir(&self.root)?;
        let base = ka_agent_core::layout::base_dir();
        if self.root.starts_with(&base) {
            restrict_dir(&base)?;
        }
        Ok(())
    }

    /// Whether the data root (or the `~/.ka-agent` base above it) lets
    /// other users in — `None` when private.
    pub fn loose_permissions(&self) -> Option<PathBuf> {
        let base = ka_agent_core::layout::base_dir();
        let mut dirs = vec![self.root.clone()];
        if self.root.starts_with(&base) {
            dirs.push(base);
        }
        dirs.into_iter().find(|d| is_loose(d))
    }
}

/// Exclusive hold on one data root: the daemon and a local chat both build a
/// `Runtime` over the same `state.db`, session and strand, so only one of
/// them may run at a time. Released when dropped (or the process dies).
pub struct InstanceLock {
    _file: std::fs::File,
}

#[derive(Debug, thiserror::Error)]
pub enum LockError {
    #[error("another Ka Agent is running on {0}")]
    Held(PathBuf),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

impl InstanceLock {
    /// Take the lock, creating the root and the lock file when missing.
    pub fn acquire(paths: &Paths) -> Result<Self, LockError> {
        std::fs::create_dir_all(paths.root())?;
        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(paths.instance_lock())?;
        match file.try_lock() {
            Ok(()) => Ok(Self { _file: file }),
            Err(std::fs::TryLockError::WouldBlock) => {
                Err(LockError::Held(paths.root().to_path_buf()))
            }
            Err(std::fs::TryLockError::Error(e)) => Err(LockError::Io(e)),
        }
    }

    /// Whether another process holds the lock (acquire-and-drop probe).
    /// I/O failures (no root yet, unreadable) count as not held.
    pub fn is_held(paths: &Paths) -> bool {
        if !paths.instance_lock().exists() {
            return false;
        }
        matches!(Self::acquire(paths), Err(LockError::Held(_)))
    }
}

#[cfg(unix)]
fn restrict_dir(dir: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    if is_loose(dir) {
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

#[cfg(not(unix))]
fn restrict_dir(_dir: &Path) -> std::io::Result<()> {
    Ok(())
}

#[cfg(unix)]
fn is_loose(dir: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(dir).is_ok_and(|m| m.permissions().mode() & 0o077 != 0)
}

#[cfg(not(unix))]
fn is_loose(_dir: &Path) -> bool {
    false
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

    #[test]
    fn instance_lock_is_exclusive_until_dropped() {
        let dir = tempfile::tempdir().unwrap();
        let p = Paths::new(dir.path());
        assert!(!InstanceLock::is_held(&p));
        let held = InstanceLock::acquire(&p).unwrap();
        assert!(InstanceLock::is_held(&p));
        assert!(matches!(InstanceLock::acquire(&p), Err(LockError::Held(_))));
        drop(held);
        assert!(!InstanceLock::is_held(&p));
        InstanceLock::acquire(&p).unwrap();
    }
}

impl Paths {
    /// Keep `<logs>/daemon.log` bounded: past `max_bytes` it becomes
    /// `daemon.log.1` (keeping two older ones). Called at start and from
    /// the maintenance loop — launchd and `run >log` write to that file
    /// unbounded otherwise (Linux logs to the journal, which rotates).
    pub fn rotate_log(&self, max_bytes: u64) -> std::io::Result<bool> {
        let path = self.logs().join("daemon.log");
        let Ok(meta) = std::fs::metadata(&path) else {
            return Ok(false);
        };
        if meta.len() < max_bytes {
            return Ok(false);
        }
        let dir = self.logs();
        let _ = std::fs::remove_file(dir.join("daemon.log.3"));
        for n in (1..=2).rev() {
            let from = dir.join(format!("daemon.log.{n}"));
            let to = dir.join(format!("daemon.log.{}", n + 1));
            if from.exists() {
                std::fs::rename(from, to)?;
            }
        }
        std::fs::rename(&path, dir.join("daemon.log.1"))?;
        Ok(true)
    }
}

#[cfg(test)]
mod rotate_tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    #[test]
    fn a_big_log_rotates_and_keeps_two_generations() {
        let dir = std::env::temp_dir().join(format!(
            "ka-rot-{}-{}",
            std::process::id(),
            ka_agent_core::now_ms()
        ));
        let paths = Paths::new(dir.join("root"));
        paths.ensure_dirs().unwrap();
        let log = paths.logs().join("daemon.log");
        std::fs::write(&log, "0".repeat(5 * 1024 * 1024 + 1)).unwrap();
        std::fs::write(paths.logs().join("daemon.log.1"), "one").unwrap();
        std::fs::write(paths.logs().join("daemon.log.2"), "two").unwrap();
        assert!(paths.rotate_log(5 * 1024 * 1024).unwrap());
        assert!(paths.logs().join("daemon.log.1").is_file());
        assert_eq!(
            std::fs::read_to_string(paths.logs().join("daemon.log.2")).unwrap(),
            "one"
        );
        assert_eq!(
            std::fs::read_to_string(paths.logs().join("daemon.log.3")).unwrap(),
            "two"
        );
        assert!(!log.exists(), "the live log starts fresh on the next write");
        // Small logs are left alone.
        std::fs::write(&log, "small").unwrap();
        assert!(!paths.rotate_log(5 * 1024 * 1024).unwrap());
        let _ = std::fs::remove_dir_all(dir);
    }
}
