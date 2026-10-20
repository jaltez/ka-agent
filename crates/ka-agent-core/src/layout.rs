//! Instance directory layout — where Ka Agent lives on disk, in the shape
//! other agents use (`~/.ka-agent/config.toml`, `~/.ka-agent/profiles/<name>/…`).
//!
//! Precedence, most explicit first:
//!
//! | Root        | `KA_AGENT_DATA_DIR` (verbatim — a self-contained instance;
//! |             | tests, service units) else `<base>/profiles/<profile>`
//! | Base        | `KA_AGENT_HOME` else `~/.ka-agent`
//! | Profile     | `KA_AGENT_PROFILE` else `default`
//! | Config      | `KA_AGENT_CONFIG` else `<data root>/config.toml` when
//! |             | `KA_AGENT_DATA_DIR` is set, else `<base>/config.toml`
//!
//! The pre-0.3 locations (`~/.local/share/ka-agent`, `~/.config/ka-agent/`)
//! are migrated on first run by the binary (`apps/ka-agent/src/layout.rs`).

use std::path::{Path, PathBuf};

/// `~/.ka-agent` or `$KA_AGENT_HOME`.
pub fn base_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("KA_AGENT_HOME")
        && !dir.is_empty()
    {
        return PathBuf::from(dir);
    }
    let home = std::env::var_os("HOME").unwrap_or_default();
    Path::new(&home).join(".ka-agent")
}

/// The profile name in use (`$KA_AGENT_PROFILE`, else `default`).
pub fn profile_name() -> String {
    std::env::var("KA_AGENT_PROFILE")
        .ok()
        .filter(|p| !p.is_empty() && p != "." && p != ".." && !p.contains('/'))
        .unwrap_or_else(|| "default".to_string())
}

/// `<base>/profiles/<profile>` (pure — testable without env games).
pub fn compose_profile_root(base: &Path, profile: &str) -> PathBuf {
    base.join("profiles").join(profile)
}

/// The default data root: `$KA_AGENT_DATA_DIR` verbatim, else
/// `<base>/profiles/<profile>`.
pub fn default_root() -> PathBuf {
    if let Some(dir) = std::env::var_os("KA_AGENT_DATA_DIR")
        && !dir.is_empty()
    {
        return PathBuf::from(dir);
    }
    compose_profile_root(&base_dir(), &profile_name())
}

/// The pre-0.3 data root (`$XDG_DATA_HOME/ka-agent`), for migration.
pub fn legacy_data_root() -> PathBuf {
    let base = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            let home = std::env::var_os("HOME").unwrap_or_default();
            Path::new(&home).join(".local/share")
        });
    base.join("ka-agent")
}

/// The pre-0.3 config path (`$XDG_CONFIG_HOME/ka-agent/ka-agent.toml`).
pub fn legacy_config_path() -> PathBuf {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            let home = std::env::var_os("HOME").unwrap_or_default();
            Path::new(&home).join(".config")
        });
    base.join("ka-agent/ka-agent.toml")
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[test]
    fn profile_roots_nest_under_base() {
        assert_eq!(
            compose_profile_root(Path::new("/home/me/.ka-agent"), "default"),
            PathBuf::from("/home/me/.ka-agent/profiles/default")
        );
        assert_eq!(
            compose_profile_root(Path::new("/x"), "work"),
            PathBuf::from("/x/profiles/work")
        );
    }
}
