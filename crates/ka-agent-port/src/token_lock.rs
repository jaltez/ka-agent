//! One bot token, one poller: an exclusive `~/.ka-agent/locks/<kind>-<hash>`
//! file per credential. A second daemon using the same token would fight
//! it at the provider (Telegram answers 409 and updates are stolen), so it
//! refuses to start the channel and names the existing holder.

use std::io::Write as _;
use std::path::PathBuf;

/// Take the lock. `Err(existing_owner)` when it is already held: the
/// profile name the holder wrote into the file (or `-`).
pub fn acquire(
    kind: &str,
    secret: &str,
    owner: &str,
    base: &std::path::Path,
) -> Result<(), String> {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(secret.as_bytes());
    let hex: String = digest.iter().take(8).map(|b| format!("{b:02x}")).collect();
    let dir = base.join("locks");
    std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let path: PathBuf = dir.join(format!("{kind}-{hex}"));
    match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
    {
        Ok(mut f) => {
            let _ = writeln!(f, "{owner}");
            Ok(())
        }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            let holder = std::fs::read_to_string(&path)
                .ok()
                .filter(|s| !s.trim().is_empty())
                .map(|s| s.trim().to_string())
                .unwrap_or_else(|| "-".into());
            Err(format!(
                "the {kind} credential is already used by profile {holder} — one token can only be polled by one daemon (they would steal each other's updates)"
            ))
        }
        Err(e) => Err(format!("{}: {e}", path.display())),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    fn base(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "ka-tlock-{tag}-{}-{}",
            std::process::id(),
            ka_agent_core::now_ms()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn one_holder_per_credential_and_it_names_them() {
        let base = base("one");
        acquire("telegram", "123:abc", "work", &base).unwrap();
        let err = acquire("telegram", "123:abc", "home", &base).unwrap_err();
        assert!(err.contains("profile work"), "{err}");
        // A different token is a different lock…
        acquire("telegram", "123:def", "home", &base).unwrap();
        // …and so is another credential kind with the same bytes.
        acquire("matrix", "123:abc", "home", &base).unwrap();
        let _ = std::fs::remove_dir_all(&base);
    }
}
