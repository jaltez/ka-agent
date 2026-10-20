//! The minimal skills mechanism (SEED.md §9.1, §2.6): ecosystem-standard
//! `SKILL.md` directories the operator owns in `~/.ka-agent/skills/`,
//! provisioned (copied) into the engine home on `ka-agent skills enable`.
//!
//! Discovery is ka's own (`$HOME/.config/ka/skills`, progressive
//! disclosure: name + description in the prompt, body read on demand), so
//! enabling is a plain copy and disabling a plain remove. Invocation is
//! ka-agent's `/skill <name> <text>`: the runtime submits the turn with
//! that one skill, and the engine injects the full body for it
//! (`Command::Prompt.skills`; the TUI's `/skill:<name>` interception does
//! not exist headless, so the channel router drives it).
//!
//! A skill is text, not code (SEED.md §2.6): it changes what the model
//! knows how to do, never what it is allowed to do.

use std::path::{Path, PathBuf};

use thiserror::Error;

#[derive(Debug, Error)]
pub enum Error {
    #[error("skill name {0:?}: [a-z0-9-] only, 1..=48 chars")]
    BadName(String),
    #[error("skill {name:?}: no SKILL.md in {dir}")]
    NoSkillMd { name: String, dir: PathBuf },
    #[error("skill {name:?} too large: {bytes} bytes (cap {cap})")]
    TooLarge { name: String, bytes: u64, cap: u64 },
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

/// Total size cap per provisioned skill directory. Skills are instructions,
/// not data; something bigger is in the wrong place.
const MAX_SKILL_BYTES: u64 = 256 * 1024;
/// At most this many files per skill directory.
const MAX_SKILL_FILES: u64 = 64;
/// How much of SKILL.md is read for the frontmatter description.
const DESC_SCAN_BYTES: u64 = 4096;

/// One skill as the surfaces list it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillInfo {
    pub name: String,
    /// Frontmatter `description` line, when present.
    pub description: String,
}

/// The two directories that matter: where the operator keeps skills and
/// where the engine discovers them.
#[derive(Debug)]
pub struct Skills {
    user_dir: PathBuf,
    engine_dir: PathBuf,
}

pub fn valid_skill_name(s: &str) -> bool {
    (1..=48).contains(&s.len())
        && s.chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

/// The operator's skill source directory: `~/.ka-agent/skills`
/// (`$KA_AGENT_HOME`-aware; `$XDG_CONFIG_HOME/ka-agent/skills` still wins
/// when it exists, for pre-0.3 installs).
pub fn user_skills_dir() -> PathBuf {
    let legacy = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            let home = std::env::var_os("HOME").unwrap_or_default();
            PathBuf::from(home).join(".config")
        })
        .join("ka-agent/skills");
    if legacy.is_dir() {
        return legacy;
    }
    ka_agent_core::layout::base_dir().join("skills")
}

impl Skills {
    pub fn new(engine_home: &Path) -> Self {
        Self {
            user_dir: user_skills_dir(),
            engine_dir: engine_home.join(".config/ka/skills"),
        }
    }

    fn scan(dir: &Path) -> Vec<SkillInfo> {
        let mut out = Vec::new();
        let Ok(entries) = std::fs::read_dir(dir) else {
            return out;
        };
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if !valid_skill_name(&name) {
                continue;
            }
            let path = entry.path().join("SKILL.md");
            if !path.is_file() {
                continue;
            }
            out.push(SkillInfo {
                name,
                description: read_description(&path),
            });
        }
        out.sort_by(|a, b| a.name.cmp(&b.name));
        out
    }

    /// Skills available to enable (in the operator's config directory).
    pub fn available(&self) -> Vec<SkillInfo> {
        Self::scan(&self.user_dir)
    }

    /// Skills the engine currently discovers (provisioned in engine home).
    pub fn enabled(&self) -> Vec<SkillInfo> {
        Self::scan(&self.engine_dir)
    }

    pub fn is_enabled(&self, name: &str) -> Result<bool, Error> {
        if !valid_skill_name(name) {
            return Err(Error::BadName(name.to_string()));
        }
        Ok(self.engine_dir.join(name).join("SKILL.md").is_file())
    }

    /// Copy one skill directory into the engine home. `Ok(false)` when the
    /// operator has no such skill. The copy is bounded (files + bytes) and
    /// replaces any earlier provisioning atomically-ish: stage, then swap.
    pub fn enable(&self, name: &str) -> Result<bool, Error> {
        if !valid_skill_name(name) {
            return Err(Error::BadName(name.to_string()));
        }
        let src = self.user_dir.join(name);
        if !src.join("SKILL.md").is_file() {
            return Ok(false);
        }
        self.check_bounds(name, &src)?;
        std::fs::create_dir_all(&self.engine_dir)?;
        let dst = self.engine_dir.join(name);
        let staging = self.engine_dir.join(format!("{name}.enabling"));
        let _ = std::fs::remove_dir_all(&staging);
        copy_dir(&src, &staging)?;
        if let Err(e) = std::fs::rename(&staging, &dst) {
            // Destination exists (rename onto a non-empty dir fails on
            // Linux): remove the old provisioning, then swap.
            if dst.exists() {
                std::fs::remove_dir_all(&dst)?;
                std::fs::rename(&staging, &dst)?;
            } else {
                let _ = std::fs::remove_dir_all(&staging);
                return Err(e.into());
            }
        }
        Ok(true)
    }

    /// Remove one skill from the engine home. `Ok(false)` when it was not
    /// enabled. The operator's source copy is never touched.
    pub fn disable(&self, name: &str) -> Result<bool, Error> {
        if !valid_skill_name(name) {
            return Err(Error::BadName(name.to_string()));
        }
        let dst = self.engine_dir.join(name);
        if !dst.is_dir() {
            return Ok(false);
        }
        std::fs::remove_dir_all(&dst)?;
        Ok(true)
    }

    fn check_bounds(&self, name: &str, src: &Path) -> Result<(), Error> {
        let (files, bytes) = dir_size(src);
        if bytes > MAX_SKILL_BYTES {
            return Err(Error::TooLarge {
                name: name.to_string(),
                bytes,
                cap: MAX_SKILL_BYTES,
            });
        }
        if files > MAX_SKILL_FILES {
            return Err(Error::TooLarge {
                name: name.to_string(),
                bytes: files,
                cap: MAX_SKILL_FILES,
            });
        }
        Ok(())
    }
}

fn copy_dir(src: &Path, dst: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let path = entry.path();
        let to = dst.join(entry.file_name());
        if path.is_dir() {
            copy_dir(&path, &to)?;
        } else {
            std::fs::copy(&path, &to)?;
        }
    }
    Ok(())
}

fn dir_size(dir: &Path) -> (u64, u64) {
    let mut files = 0u64;
    let mut bytes = 0u64;
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                let (f, b) = dir_size(&path);
                files += f;
                bytes += b;
            } else if let Ok(md) = entry.metadata() {
                files += 1;
                bytes += md.len();
            }
        }
    }
    (files, bytes)
}

/// The frontmatter `description = "…"` line, best-effort. Progressive
/// disclosure shows one line per skill; a missing description is fine.
fn read_description(skill_md: &Path) -> String {
    use std::io::Read;
    let mut buf = vec![0u8; DESC_SCAN_BYTES as usize];
    let text = std::fs::File::open(skill_md)
        .and_then(|mut f| {
            let n = f.read(&mut buf)?;
            Ok(String::from_utf8_lossy(&buf[..n]).into_owned())
        })
        .unwrap_or_default();
    for line in text.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("description") {
            let rest = rest.trim_start();
            if let Some(v) = rest.strip_prefix('=') {
                return v
                    .trim()
                    .trim_matches('"')
                    .trim_matches('\'')
                    .chars()
                    .take(120)
                    .collect();
            }
        }
    }
    String::new()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    fn skill_dir(base: &Path, name: &str, description: &str) {
        let dir = base.join(name);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("SKILL.md"),
            format!("---\nname = {name}\ndescription = \"{description}\"\n---\n\nDo the thing.\n"),
        )
        .unwrap();
    }

    #[test]
    fn enable_disable_list_roundtrip() {
        let tmp = tempfile::tempdir().unwrap();
        let user = tmp.path().join("user-skills");
        let engine_home = tmp.path().join("engine-home");
        std::fs::create_dir_all(&user).unwrap();
        skill_dir(&user, "review", "review a diff carefully");
        skill_dir(&user, "Bad_Name", "invalid name, ignored");

        // Scoped scan: invalid names are skipped, description is read.
        let available = Skills::scan(&user);
        assert_eq!(available.len(), 1, "invalid names are skipped");
        assert_eq!(available[0].name, "review");
        assert_eq!(available[0].description, "review a diff carefully");

        let skills = Skills {
            user_dir: user.clone(),
            engine_dir: engine_home.join(".config/ka/skills"),
        };
        assert!(!skills.is_enabled("review").unwrap());
        assert!(skills.enable("review").unwrap());
        assert!(skills.is_enabled("review").unwrap());
        let enabled = skills.enabled();
        assert_eq!(enabled, available);
        // Re-enable replaces cleanly.
        assert!(skills.enable("review").unwrap());
        assert_eq!(skills.enabled().len(), 1);
        // Disable only removes the provisioning.
        assert!(skills.disable("review").unwrap());
        assert!(!skills.is_enabled("review").unwrap());
        assert!(user.join("review/SKILL.md").is_file());
        assert!(!skills.disable("review").unwrap());
    }

    #[test]
    fn enable_unknown_or_invalid_fails_closed() {
        let tmp = tempfile::tempdir().unwrap();
        let skills = Skills {
            user_dir: tmp.path().join("user"),
            engine_dir: tmp.path().join("engine/.config/ka/skills"),
        };
        assert!(!skills.enable("ghost").unwrap());
        assert!(skills.enable("../escape").is_err());
        assert!(skills.is_enabled("Nope").is_err());
        assert!(!skills.disable("ghost").unwrap());
    }

    #[test]
    fn oversized_skill_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let user = tmp.path().join("user");
        let dir = user.join("big");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("SKILL.md"), "# big\n").unwrap();
        std::fs::write(
            dir.join("blob.bin"),
            vec![0u8; (MAX_SKILL_BYTES + 1) as usize],
        )
        .unwrap();
        let skills = Skills {
            user_dir: user,
            engine_dir: tmp.path().join("engine/.config/ka/skills"),
        };
        assert!(skills.enable("big").is_err());
    }
}
