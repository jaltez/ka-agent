//! Instance directory layout — where Ka Agent lives on disk, in the shape
//! other agents use (`~/.ka-agent/config.toml`, `~/.ka-agent/profiles/<name>/…`).
//!
//! Precedence, most explicit first:
//!
//! | Root        | `KA_AGENT_DATA_DIR` (verbatim — a self-contained instance;
//! |             | tests, service units) else `<base>/profiles/<profile>`
//! | Base        | `KA_AGENT_HOME` else `~/.ka-agent`
//! | Profile     | `KA_AGENT_PROFILE` else `default`
//! | Profile     | `-p/--profile`, `KA_AGENT_PROFILE`, `profile use`, `default`
//! | Config      | `KA_AGENT_CONFIG` else `<data root>/config.toml` when
//! |             | `KA_AGENT_DATA_DIR` is set, else `<base>/config.toml` for
//! |             | the default profile, `<base>/profiles/<p>/config.toml` else
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

/// Process-wide profile chosen on the command line (`-p/--profile`).
/// Set once by the binary before anything resolves a path.
static PROFILE_OVERRIDE: std::sync::OnceLock<String> = std::sync::OnceLock::new();

/// Pin the profile for this process (`ka-agent -p work …`). First call wins.
pub fn set_profile_override(name: &str) {
    let _ = PROFILE_OVERRIDE.set(name.to_string());
}

/// A usable profile name: `[a-z0-9-]`, 1..=32 chars. The same alphabet
/// works for directories, systemd unit names and launchd labels.
pub fn valid_profile_name(name: &str) -> bool {
    (1..=32).contains(&name.len())
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        && !name.starts_with('-')
}

/// The file that remembers `ka-agent profile use <name>`.
pub fn active_profile_file() -> PathBuf {
    base_dir().join("active_profile")
}

/// The profile in use: `-p/--profile`, else `$KA_AGENT_PROFILE`, else the
/// one chosen with `ka-agent profile use`, else `default`.
pub fn profile_name() -> String {
    if let Some(p) = PROFILE_OVERRIDE.get() {
        return p.clone();
    }
    if let Ok(p) = std::env::var("KA_AGENT_PROFILE")
        && valid_profile_name(&p)
    {
        return p;
    }
    std::fs::read_to_string(active_profile_file())
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|p| valid_profile_name(p))
        .unwrap_or_else(|| "default".to_string())
}

/// Why the profile selection outside `-p` cannot be trusted, if it
/// cannot: an invalid `$KA_AGENT_PROFILE`, or an `active_profile` file
/// naming a profile that is gone. Silently falling back to `default`
/// would run commands against the wrong instance.
pub fn profile_selection_problem() -> Option<String> {
    if PROFILE_OVERRIDE.get().is_some() {
        return None;
    }
    if let Ok(p) = std::env::var("KA_AGENT_PROFILE")
        && !p.is_empty()
    {
        return (!valid_profile_name(&p)).then(|| {
            format!(
                "KA_AGENT_PROFILE={p:?} is not a valid profile name ([a-z0-9-], up to 32 chars)"
            )
        });
    }
    let file = active_profile_file();
    let p = std::fs::read_to_string(&file).ok()?.trim().to_string();
    if p == "default" {
        return None;
    }
    if !valid_profile_name(&p) || !compose_profile_root(&base_dir(), &p).is_dir() {
        return Some(format!(
            "active profile {p:?} (from {}) does not exist — `ka-agent profile use default` or `ka-agent profile create {p}`",
            file.display()
        ));
    }
    None
}

/// Every profile directory under `<base>/profiles`, sorted (`default`
/// included once it exists). A directory only counts with an identity
/// file — its config or state.db — so a stray `profiles/foo/logs` (a
/// crashed `create`, a manual copy) never shows up as a profile.
pub fn list_profiles() -> Vec<String> {
    let profiles = base_dir().join("profiles");
    let mut out: Vec<String> = std::fs::read_dir(&profiles)
        .map(|d| {
            d.flatten()
                .filter(|e| e.path().is_dir())
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .filter(|n| {
                    valid_profile_name(n)
                        && (profiles.join(n).join("config.toml").is_file()
                            || profiles.join(n).join("state.db").is_file())
                })
                .collect()
        })
        .unwrap_or_default();
    out.sort();
    out
}

/// A profile's config file: the default profile's lives at the base
/// (`~/.ka-agent/config.toml`), every other profile owns one inside its
/// directory, so profiles never share channels, ports or models by
/// accident.
pub fn config_path_for(profile: &str) -> PathBuf {
    if profile == "default" {
        base_dir().join("config.toml")
    } else {
        compose_profile_root(&base_dir(), profile).join("config.toml")
    }
}

/// The config file for the current process: `$KA_AGENT_CONFIG`, else
/// `<data root>/config.toml` when the root is pinned with
/// `$KA_AGENT_DATA_DIR` (a self-contained instance), else the active
/// profile's ([`config_path_for`]).
pub fn default_config_path() -> PathBuf {
    if let Some(p) = std::env::var_os("KA_AGENT_CONFIG")
        && !p.is_empty()
    {
        return PathBuf::from(p);
    }
    if let Some(dir) = std::env::var_os("KA_AGENT_DATA_DIR")
        && !dir.is_empty()
    {
        return PathBuf::from(dir).join("config.toml");
    }
    config_path_for(&profile_name())
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
    fn profile_names_are_dir_and_unit_safe() {
        for ok in ["default", "work", "home-2"] {
            assert!(valid_profile_name(ok), "{ok}");
        }
        for bad in ["", "Work", "a/b", "..", "-x", "a b", &"x".repeat(33)] {
            assert!(!valid_profile_name(bad), "{bad}");
        }
    }

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
