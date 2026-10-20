//! One-time migration from the pre-0.3 layout (`~/.local/share/ka-agent` +
//! `~/.config/ka-agent/ka-agent.toml`) to the agent-standard one
//! (`~/.ka-agent/config.toml` + `~/.ka-agent/profiles/default/…`).
//!
//! Runs only when the new base does not exist yet and no env override
//! (`KA_AGENT_HOME`, `KA_AGENT_DATA_DIR`) points elsewhere: an explicit
//! override means the operator owns the layout. Everything moves with one
//! `rename` (same device in practice); anything else stops with the exact
//! `mv` to run — data is never copied-and-deleted by surprise.

use std::path::Path;

use ka_agent_core::layout;

/// Migrate, at most once. `Ok(())` also when there is nothing to do.
pub fn migrate() -> Result<(), String> {
    if std::env::var_os("KA_AGENT_HOME").is_some_and(|v| !v.is_empty())
        || std::env::var_os("KA_AGENT_DATA_DIR").is_some_and(|v| !v.is_empty())
    {
        return Ok(());
    }
    let base = layout::base_dir();
    if base.exists() {
        return Ok(());
    }
    let old_root = layout::legacy_data_root();
    let old_config = layout::legacy_config_path();
    if !old_root.is_dir() && !old_config.is_file() {
        return Ok(());
    }
    let profile_root = layout::compose_profile_root(&base, &layout::profile_name());

    if old_root.is_dir() {
        let parent = profile_root
            .parent()
            .ok_or_else(|| format!("no parent for {}", profile_root.display()))?;
        std::fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
        match std::fs::rename(&old_root, &profile_root) {
            Ok(()) => {}
            Err(e) if e.raw_os_error() == Some(libc_exdev()) => {
                return Err(format!(
                    "cannot move {} across devices automatically — run: mv {} {}",
                    old_root.display(),
                    old_root.display(),
                    profile_root.display()
                ));
            }
            Err(e) => {
                return Err(format!("cannot move {}: {e}", old_root.display()));
            }
        }
    }

    // The config moves to <base>/config.toml; an absolute workspace under
    // the old root follows the data.
    if old_config.is_file() {
        let target = base.join("config.toml");
        std::fs::create_dir_all(&base).map_err(|e| format!("{}: {e}", base.display()))?;
        if !target.exists() {
            rewrite_workspace_prefix(&old_config, &old_root, &profile_root)?;
            std::fs::rename(&old_config, &target)
                .map_err(|e| format!("cannot move {}: {e}", old_config.display()))?;
        }
        let _ = std::fs::remove_dir(old_config.parent().unwrap_or(Path::new("/")));
    }
    Ok(())
}

/// `EXDEV` without a libc dependency (18 on Linux, 66 on macOS, 17? — the
/// constants differ, so probe by name once).
fn libc_exdev() -> i32 {
    #[cfg(target_os = "macos")]
    {
        66
    }
    #[cfg(target_os = "linux")]
    {
        18
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        -1
    }
}

/// Rewrite a `workspace = "<old root>/…"` value (or default-shaped one) to
/// the new profile root, format-preserving. Failures are reported, not
/// swallowed: a half-migrated config pointing into a moved tree is worse.
fn rewrite_workspace_prefix(config: &Path, old_root: &Path, new_root: &Path) -> Result<(), String> {
    let doc = ka_agent_core::config_file::read_doc(config).map_err(|e| e.to_string())?;
    let Some(value) = doc.get("workspace").and_then(|v| v.as_str()) else {
        return Ok(());
    };
    let expanded = ka_agent_core::expand_home(value);
    let Ok(rest) = expanded.strip_prefix(old_root) else {
        return Ok(());
    };
    let new_workspace = new_root.join(rest);
    ka_agent_core::config_file::set(
        config,
        "workspace",
        &format!("{:?}", new_workspace.display().to_string()),
    )
    .map_err(|e| e.to_string())?;
    Ok(())
}
