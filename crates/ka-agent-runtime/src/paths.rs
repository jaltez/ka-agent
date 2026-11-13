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

    /// The `users/` tree every non-operator user owns a subdirectory of.
    pub fn users_root(&self) -> PathBuf {
        self.root.join("users")
    }

    /// One user's root: the operator keeps the profile root itself (zero
    /// migration); everyone else owns `<root>/users/<username>`.
    pub fn user_root(&self, username: &str) -> PathBuf {
        if username == ka_agent_core::OPERATOR_USER {
            self.root.clone()
        } else {
            self.users_root().join(username)
        }
    }

    /// The user's memory root (disjoint from every other user's).
    pub fn user_memory(&self, username: &str) -> PathBuf {
        self.user_root(username).join("memory")
    }

    /// The user's notes directory (`kagent__notes_*`).
    pub fn user_notes(&self, username: &str) -> PathBuf {
        self.user_root(username).join("notes")
    }

    /// The user's attachments (engine files, private-session media).
    pub fn user_attachments(&self, username: &str) -> PathBuf {
        self.user_root(username).join("attachments")
    }

    /// The user's synthetic HOME (compiled MEMORY.md/IDENTITY.md, skills).
    pub fn user_engine_home(&self, username: &str) -> PathBuf {
        self.user_root(username).join("engine-home")
    }

    /// The user's XDG_STATE_HOME (spills, trust store, detached jobs).
    pub fn user_engine_state(&self, username: &str) -> PathBuf {
        self.user_root(username).join("engine-state")
    }

    /// The user's `KA_DATA_DIR` (strands, waypoints, index).
    pub fn user_ka_data(&self, username: &str) -> PathBuf {
        self.user_root(username).join("ka")
    }

    /// The user's XDG_CACHE_HOME (feeds the sandbox allowlist).
    pub fn user_engine_cache(&self, username: &str) -> PathBuf {
        self.user_engine_home(username).join(".cache")
    }

    /// The user's default workspace (`<user root>/workspace`).
    pub fn user_workspace(&self, username: &str) -> PathBuf {
        self.user_root(username).join("workspace")
    }

    /// Create one user's tree, 0700 like the rest. Idempotent; the operator
    /// is the profile root already covered by [`Self::ensure_dirs`].
    pub fn ensure_user_dirs(&self, username: &str) -> std::io::Result<()> {
        if username == ka_agent_core::OPERATOR_USER {
            return self.ensure_dirs();
        }
        for dir in [
            self.user_root(username),
            self.user_memory(username),
            self.user_workspace(username),
            self.user_notes(username),
            self.user_attachments(username),
            self.user_ka_data(username),
            self.user_engine_home(username).join(".config/ka"),
            self.user_engine_cache(username),
            self.user_engine_state(username),
        ] {
            std::fs::create_dir_all(dir)?;
        }
        restrict_dir(&self.user_root(username))?;
        restrict_dir(&self.users_root())?;
        Ok(())
    }

    /// Move the pre-users `<root>/roles/<name>` trees to their user
    /// partitions (`<root>/users/<name>`) once. Idempotent: a tree moves only
    /// when the source exists and the target does not, and a failed move
    /// leaves the source in place. Returns what moved.
    pub fn migrate_role_trees(&self) -> std::io::Result<Vec<(String, PathBuf)>> {
        let roles = self.root.join("roles");
        let users = self.users_root();
        let mut moved = Vec::new();
        let entries = match std::fs::read_dir(&roles) {
            Ok(e) => e,
            Err(_) => return Ok(moved),
        };
        for entry in entries.flatten() {
            if !entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                continue;
            }
            let name = match entry.file_name().to_str() {
                Some(n) => n.to_string(),
                None => continue,
            };
            // `admin` was never a directory (it is the profile root).
            if name == ka_agent_core::ADMIN_ROLE {
                continue;
            }
            let target = users.join(&name);
            if target.exists() {
                continue;
            }
            std::fs::create_dir_all(&users)?;
            match std::fs::rename(entry.path(), &target) {
                Ok(()) => moved.push((name, target)),
                Err(_) => {
                    // Different filesystems: copy then remove the source. A
                    // failed copy leaves a partial target that would block
                    // every later retry, so remove it and surface the error.
                    if let Err(e) = copy_tree(&entry.path(), &target) {
                        let _ = std::fs::remove_dir_all(&target);
                        return Err(e);
                    }
                    let _ = std::fs::remove_dir_all(entry.path());
                    moved.push((name, target));
                }
            }
        }
        Ok(moved)
    }

    /// One role's root: `admin` is the profile root itself; every other role
    /// owns `<root>/roles/<name>`. Roles are permission sets now, but the
    /// pre-users trees still exist and are moved to `users/` at bootstrap.
    pub fn role_root(&self, role: &str) -> PathBuf {
        if role == ka_agent_core::ADMIN_ROLE {
            self.root.clone()
        } else {
            self.root.join("roles").join(role)
        }
    }

    /// The role's memory root (disjoint from every other role's).
    pub fn role_memory(&self, role: &str) -> PathBuf {
        self.role_root(role).join("memory")
    }

    /// The role's notes directory (`kagent__notes_*`).
    pub fn role_notes(&self, role: &str) -> PathBuf {
        self.role_root(role).join("notes")
    }

    /// The role's attachments (engine files, private-session media).
    pub fn role_attachments(&self, role: &str) -> PathBuf {
        self.role_root(role).join("attachments")
    }

    /// The role's synthetic HOME (compiled MEMORY.md/IDENTITY.md, skills).
    pub fn role_engine_home(&self, role: &str) -> PathBuf {
        self.role_root(role).join("engine-home")
    }

    /// The role's XDG_STATE_HOME (spills, trust store, detached jobs).
    pub fn role_engine_state(&self, role: &str) -> PathBuf {
        self.role_root(role).join("engine-state")
    }

    /// The role's `KA_DATA_DIR` (strands, waypoints, index).
    pub fn role_ka_data(&self, role: &str) -> PathBuf {
        self.role_root(role).join("ka")
    }

    /// The role's XDG_CACHE_HOME (feeds the sandbox allowlist).
    pub fn role_engine_cache(&self, role: &str) -> PathBuf {
        self.role_engine_home(role).join(".cache")
    }

    /// The role's default workspace (`<role root>/workspace`); a role may
    /// pin an explicit `workspace` in its config instead.
    pub fn role_workspace(&self, role: &str) -> PathBuf {
        self.role_root(role).join("workspace")
    }

    /// Create one role's tree (`memory/`, `workspace/`, `notes/`,
    /// `attachments/`, `engine-home/.config/ka`, `engine-state/`), 0700
    /// like the rest. Idempotent.
    pub fn ensure_role_dirs(&self, role: &str) -> std::io::Result<()> {
        if role == ka_agent_core::ADMIN_ROLE {
            return self.ensure_dirs();
        }
        for dir in [
            self.role_root(role),
            self.role_memory(role),
            self.role_workspace(role),
            self.role_notes(role),
            self.role_attachments(role),
            self.role_ka_data(role),
            self.role_engine_home(role).join(".config/ka"),
            self.role_engine_cache(role),
            self.role_engine_state(role),
        ] {
            std::fs::create_dir_all(dir)?;
        }
        restrict_dir(&self.role_root(role))?;
        restrict_dir(&self.root.join("roles"))?;
        Ok(())
    }

    /// Every configured role's tree (admin's is the profile root, already
    /// covered by [`Self::ensure_dirs`]).
    pub fn ensure_role_dirs_for(&self, roles: &[ka_agent_core::RoleSpec]) -> std::io::Result<()> {
        for role in roles {
            self.ensure_role_dirs(&role.name)?;
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

/// Recursive copy (the fallback when a rename crosses filesystems).
fn copy_tree(from: &Path, to: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(to)?;
    for entry in std::fs::read_dir(from)?.flatten() {
        let path = entry.path();
        let target = to.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_tree(&path, &target)?;
        } else {
            std::fs::copy(&path, &target)?;
        }
    }
    Ok(())
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
    fn role_roots_are_disjoint_and_admin_is_the_profile_root() {
        let root = PathBuf::from("/data/profile");
        let p = Paths::new(root.clone());
        assert_eq!(p.role_root("admin"), root);
        assert_eq!(p.role_memory("admin"), root.join("memory"));
        assert_eq!(p.role_root("guest"), root.join("roles/guest"));
        assert_eq!(p.role_memory("guest"), root.join("roles/guest/memory"));
        assert_eq!(p.role_notes("guest"), root.join("roles/guest/notes"));
        assert_eq!(
            p.role_engine_home("guest"),
            root.join("roles/guest/engine-home")
        );
        assert_eq!(
            p.role_workspace("guest"),
            root.join("roles/guest/workspace")
        );
        assert_ne!(p.role_memory("guest"), p.role_memory("ops"));
    }

    #[test]
    fn ensure_role_dirs_creates_the_tree_private() {
        let root = std::env::temp_dir().join(format!("ka-agent-role-paths-{}", std::process::id()));
        let p = Paths::new(root.clone());
        p.ensure_role_dirs("guest").unwrap();
        for dir in [
            p.role_memory("guest"),
            p.role_notes("guest"),
            p.role_workspace("guest"),
            p.role_attachments("guest"),
            p.role_engine_home("guest").join(".config/ka"),
            p.role_engine_state("guest"),
        ] {
            assert!(dir.is_dir(), "{dir:?}");
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(p.role_root("guest"))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o077, 0, "role root must be 0700");
        }
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
