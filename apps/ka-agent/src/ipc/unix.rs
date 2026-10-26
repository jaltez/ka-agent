//! Unix-domain-socket IPC: the platform half of the admin transport. The
//! daemon-hosted tools (later) and the Windows named-pipe swap slot in
//! beside this file unchanged.

use std::io;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::task::{Context, Poll};

use ka_agent_runtime::paths::Paths;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::UnixStream;

/// The admin socket inside the data root (same location as always).
pub fn socket_path(paths: &Paths) -> PathBuf {
    paths.root().join("admin.sock")
}

/// Owner of the socket file; only this uid may talk to the host. Read from
/// the file itself (portable; `/proc` does not exist on macOS).
fn owner_uid(path: &Path) -> io::Result<u32> {
    Ok(std::fs::metadata(path)?.uid())
}

/// The host side: one listener on the admin socket.
pub struct IpcListener {
    listener: tokio::net::UnixListener,
    own_uid: u32,
}

impl IpcListener {
    /// Bind the admin socket (removing any stale file first — safe because
    /// the caller holds the instance lock), mode 0600.
    pub fn bind(paths: &Paths) -> io::Result<Self> {
        let sock = socket_path(paths);
        let _ = std::fs::remove_file(&sock);
        let listener = tokio::net::UnixListener::bind(&sock)?;
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&sock, std::fs::Permissions::from_mode(0o600))?;
        }
        Ok(Self {
            listener,
            own_uid: owner_uid(&sock)?,
        })
    }

    pub async fn accept(&self) -> io::Result<IpcStream> {
        let (stream, _) = self.listener.accept().await?;
        Ok(IpcStream {
            stream,
            own_uid: self.own_uid,
        })
    }
}

/// One accepted or connected stream over the admin socket.
pub struct IpcStream {
    stream: UnixStream,
    /// The socket owner's uid; unused where the platform has no
    /// peer-credential call (macOS/BSD rely on the 0600 mode).
    #[cfg_attr(not(any(target_os = "linux", target_os = "android")), allow(dead_code))]
    own_uid: u32,
}

impl IpcStream {
    /// The peer is the socket's owner. Linux/Android ask the kernel
    /// (`SO_PEERCRED`); elsewhere the socket's 0600 mode is the gate — the
    /// kernel refuses a connect from another user, and there is no safe
    /// peer-credential call for macOS without `unsafe` FFI.
    pub fn peer_ok(&self) -> bool {
        #[cfg(any(target_os = "linux", target_os = "android"))]
        {
            self.stream.peer_cred().map(|c| c.uid()).ok() == Some(self.own_uid)
        }
        #[cfg(not(any(target_os = "linux", target_os = "android")))]
        {
            true
        }
    }

    /// Split into halves (the attach stream's reader/writer). The platform-
    /// neutral aliases hide the concrete halves from callers.
    pub fn split(self) -> (super::IpcRead, super::IpcWrite) {
        tokio::io::split(self)
    }
}

impl AsyncRead for IpcStream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.stream).poll_read(cx, buf)
    }
}

impl AsyncWrite for IpcStream {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.stream).poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.stream).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.stream).poll_shutdown(cx)
    }
}

/// Connect to the host's admin socket (async).
pub async fn connect(paths: &Paths) -> io::Result<IpcStream> {
    let sock = socket_path(paths);
    let stream = UnixStream::connect(&sock).await?;
    let own_uid = owner_uid(&sock).unwrap_or(u32::MAX);
    Ok(IpcStream { stream, own_uid })
}

/// Connect from blocking code (one-shot clients such as `ka-agent status`).
pub fn connect_blocking(paths: &Paths) -> io::Result<BlockingStream> {
    let stream = std::os::unix::net::UnixStream::connect(socket_path(paths))?;
    Ok(BlockingStream { stream })
}

/// A blocking admin stream: one request line out, one reply line in.
pub struct BlockingStream {
    stream: std::os::unix::net::UnixStream,
}

impl BlockingStream {
    pub fn set_read_timeout(&self, t: Option<std::time::Duration>) -> io::Result<()> {
        self.stream.set_read_timeout(t)
    }

    pub fn set_write_timeout(&self, t: Option<std::time::Duration>) -> io::Result<()> {
        self.stream.set_write_timeout(t)
    }
}

impl io::Read for BlockingStream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.stream.read(buf)
    }
}

impl io::Write for BlockingStream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.stream.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.stream.flush()
    }
}

impl io::Read for &BlockingStream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        (&self.stream).read(buf)
    }
}

impl io::Write for &BlockingStream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        (&self.stream).write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        (&self.stream).flush()
    }
}

/// Whether a host answers on the admin socket right now. A stale socket
/// file refuses the connect, so this is the proof `restore` needs.
pub fn is_running(paths: &Paths) -> bool {
    connect_blocking(paths).is_ok()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    fn tmp_paths(tag: &str) -> Paths {
        let dir = std::env::temp_dir().join(format!("ka-agent-ipc-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Paths::new(dir)
    }

    #[tokio::test]
    async fn bind_accept_connect_and_peer_check() {
        let paths = tmp_paths("peer");
        let listener = IpcListener::bind(&paths).unwrap();
        let sock = socket_path(&paths);
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&sock).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        let server = tokio::spawn(async move {
            // Two connections: the client, then the `is_running` probe.
            for _ in 0..2 {
                let stream = listener.accept().await.unwrap();
                assert!(stream.peer_ok());
            }
        });
        let client = connect(&paths).await.unwrap();
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let mut client = client;
        client.write_all(b"ping\n").await.unwrap();
        assert!(is_running(&paths));
        let mut buf = [0u8; 5];
        let _ = client.read_exact(&mut buf).await;
        server.await.unwrap();
        let _ = std::fs::remove_dir_all(paths.root());
    }

    #[test]
    fn stale_socket_is_not_running() {
        let paths = tmp_paths("stale");
        let _ = std::fs::File::create(socket_path(&paths)).unwrap();
        assert!(!is_running(&paths));
        let _ = std::fs::remove_dir_all(paths.root());
    }
}
