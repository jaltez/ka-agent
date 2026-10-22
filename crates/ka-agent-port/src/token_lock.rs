//! One bot token, one poller: an advisory `flock` on
//! `~/.ka-agent/locks/<kind>-<hash>`. A second daemon using the same
//! credential would fight it at the provider (Telegram answers 409 and
//! updates are stolen), so it refuses to start the channel and names the
//! existing holder. The lock is held by the file descriptor — a crashed or
//! killed daemon releases it, so restarts are never blocked by a stale file.

use std::fs::File;
use std::io::Write as _;
use std::path::PathBuf;

use fs2::FileExt as _;

/// A held credential lock. Dropping it (process exit included) releases it.
#[derive(Debug)]
pub struct TokenLock {
    _file: File,
}

/// Take the lock, or name the current holder.
pub fn acquire(
    kind: &str,
    secret: &str,
    owner: &str,
    base: &std::path::Path,
) -> Result<TokenLock, String> {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(secret.as_bytes());
    let hex: String = digest.iter().take(8).map(|b| format!("{b:02x}")).collect();
    let dir = base.join("locks");
    std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let path: PathBuf = dir.join(format!("{kind}-{hex}"));
    // No truncate: the current holder's name must survive until this open
    // loses the lock race (truncation happens only after winning).
    #[allow(clippy::suspicious_open_options)]
    let mut file = File::options()
        .create(true)
        .write(true)
        .open(&path)
        .map_err(|e| format!("{}: {e}", path.display()))?;
    if let Err(e) = file.try_lock_exclusive() {
        // The holder wrote its name under the lock; no truncation happened.
        let holder = std::fs::read_to_string(&path)
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "-".into());
        return Err(format!(
            "the {kind} credential is already used by profile {holder} — one token can only be polled by one daemon (they would steal each other's updates): {e}"
        ));
    }
    let _ = file.set_len(0);
    let _ = writeln!(file, "{owner}");
    Ok(TokenLock { _file: file })
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
        let held = acquire("telegram", "123:abc", "work", &base).unwrap();
        let err = acquire("telegram", "123:abc", "home", &base).unwrap_err();
        assert!(err.contains("profile work"), "{err}");
        // A different token is a different lock…
        acquire("telegram", "123:def", "home", &base).unwrap();
        // …and so is another credential kind with the same bytes.
        acquire("matrix", "123:abc", "home", &base).unwrap();
        // A released lock (the process died) is gone for the next taker.
        drop(held);
        acquire("telegram", "123:abc", "home", &base).unwrap();
        let _ = std::fs::remove_dir_all(&base);
    }
}
