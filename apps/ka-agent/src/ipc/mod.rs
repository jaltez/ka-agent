//! Platform IPC for the runtime host: the admin endpoint every host serves
//! (daemon and local chat alike). Unix domain sockets on Unix; a named
//! pipe with the same protocol on Windows (plan 4.2). The halves are
//! crate-local aliases so nothing above this module names a platform type.

mod server;

#[cfg(unix)]
mod unix;
#[cfg(windows)]
mod windows;

#[cfg(unix)]
#[allow(unused_imports)] // named from the backends' signatures
pub use unix::{
    BlockingStream, IpcListener, IpcStream, connect, connect_blocking, is_running, socket_path,
};
#[cfg(windows)]
#[allow(unused_imports)]
pub use windows::{
    BlockingStream, IpcListener, IpcStream, connect, connect_blocking, is_running, socket_path,
};

pub(crate) use server::{AdminCtx, serve_admin};

/// The Windows pipe name for a data root: pure, so it is testable on every
/// platform (`\\.\pipe\ka-agent-<sha256(root)[..16]>`).
#[cfg_attr(not(windows), allow(dead_code))] // used by the Windows backend
pub fn pipe_name_for(root: &std::path::Path) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(root.as_os_str().to_string_lossy().as_bytes());
    let digest = hasher.finalize();
    let hex: String = digest[..8].iter().map(|b| format!("{b:02x}")).collect();
    format!(r"\\.\pipe\ka-agent-{hex}")
}

/// Read half of an attach stream (one locked stream, split in two).
pub type IpcRead = tokio::io::ReadHalf<IpcStream>;
/// Write half of an attach stream.
pub type IpcWrite = tokio::io::WriteHalf<IpcStream>;

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    #[test]
    fn pipe_names_are_stable_and_per_root() {
        let a = pipe_name_for(std::path::Path::new(r"C:\Users\ka\data-a"));
        let b = pipe_name_for(std::path::Path::new(r"C:\Users\ka\data-b"));
        assert_eq!(
            a,
            pipe_name_for(std::path::Path::new(r"C:\Users\ka\data-a")),
            "the name is stable across calls (status must find the daemon)"
        );
        assert_ne!(a, b, "different roots must not share a pipe");
        assert!(a.starts_with(r"\\.\pipe\ka-agent-"), "{a}");
        let hex = a.rsplit('-').next().unwrap();
        assert_eq!(hex.len(), 16, "16 hex chars: {hex}");
        assert!(hex.chars().all(|c| c.is_ascii_hexdigit()), "{hex}");
    }
}
