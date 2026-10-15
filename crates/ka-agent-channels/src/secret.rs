//! Secret resolution (SEED.md §8.5, file fallback tier): a secret name is
//! resolved from the environment first (`KA_AGENT_SECRET_<UPPERCASED>`),
//! then from a `0600` file under the instance's `secrets/` directory.
//! The OS keyring tier arrives later; the lookup order is already fixed.

use std::path::{Path, PathBuf};

use thiserror::Error;

#[derive(Debug, Error)]
pub enum SecretError {
    #[error("secret {name:?}: {reason}")]
    Unresolved { name: String, reason: String },
    #[error("secret {name:?}: read {path}: {source}")]
    Io {
        name: String,
        path: PathBuf,
        source: std::io::Error,
    },
}

/// Resolve a secret by name.
///
/// 1. `KA_AGENT_SECRET_<NAME>` environment variable (dots and dashes become
///    underscores), if set and non-empty.
/// 2. `<secrets_dir>/<name>` — a single-line file, `0600`, created by the
///    operator or `ka-agent secrets set`.
pub fn resolve(name: &str, secrets_dir: &Path) -> Result<String, SecretError> {
    let env_key = format!(
        "KA_AGENT_SECRET_{}",
        name.to_ascii_uppercase().replace(['.', '-'], "_")
    );
    let from_env = std::env::var_os(&env_key).map(|v| v.to_string_lossy().into_owned());
    resolve_with(name, secrets_dir, from_env.as_deref())
}

/// `resolve` with the env tier injected (edition 2024 forbids mutating the
/// environment in-process, so tests pass the would-be env value here).
fn resolve_with(
    name: &str,
    secrets_dir: &Path,
    from_env: Option<&str>,
) -> Result<String, SecretError> {
    if let Some(v) = from_env {
        let v = v.trim();
        if !v.is_empty() {
            return Ok(v.to_string());
        }
    }
    let path = secrets_dir.join(name);
    let text = std::fs::read_to_string(&path).map_err(|source| SecretError::Io {
        name: name.to_string(),
        path: path.clone(),
        source,
    })?;
    let v = text.trim().to_string();
    if v.is_empty() {
        return Err(SecretError::Unresolved {
            name: name.to_string(),
            reason: format!("{} is empty", path.display()),
        });
    }
    Ok(v)
}

/// Store a secret as a `0600` single-line file (admin CLI / setup).
pub fn store(name: &str, value: &str, secrets_dir: &Path) -> Result<PathBuf, SecretError> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::create_dir_all(secrets_dir).map_err(|source| SecretError::Io {
        name: name.to_string(),
        path: secrets_dir.to_path_buf(),
        source,
    })?;
    let path = secrets_dir.join(name);
    std::fs::write(&path, format!("{value}\n")).map_err(|source| SecretError::Io {
        name: name.to_string(),
        path: path.clone(),
        source,
    })?;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).map_err(|source| {
        SecretError::Io {
            name: name.to_string(),
            path: path.clone(),
            source,
        }
    })?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    fn tmp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("ka-agent-secret-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn env_wins_when_set() {
        // `std::env::set_var` is unsafe in edition 2024 and unsafe code is
        // forbidden crate-wide, so the env tier is exercised through
        // `resolve_with` (the same lookup the real `resolve` performs).
        let dir = tmp("env");
        let v = resolve_with("m2.env_x1", &dir, Some("  from-env  ")).unwrap();
        assert_eq!(v, "from-env");
    }

    #[test]
    fn file_fallback_with_trim_and_mode() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tmp("file");
        let p = store("telegram.bot_token", "123:abc", &dir).unwrap();
        let v = resolve("telegram.bot_token", &dir).unwrap();
        assert_eq!(v, "123:abc");
        let mode = std::fs::metadata(&p).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    #[test]
    fn missing_secret_is_an_error() {
        let dir = tmp("missing");
        let e = resolve("nope", &dir).unwrap_err();
        assert!(e.to_string().contains("nope"), "{e}");
    }
}
