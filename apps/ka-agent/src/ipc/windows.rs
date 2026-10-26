//! Windows named-pipe IPC: the platform half of the admin transport
//! (plan 4.2). Same protocol as the Unix socket — one JSON request line
//! out, one JSON reply line in — so nothing above this module changes.
//!
//! Pipe name: `\\.\pipe\ka-agent-<sha256(data root) first 16 hex>`. The
//! hash keeps profiles and data roots apart and is stable across restarts,
//! so `status` can find a running daemon the same way `is_running` does on
//! Unix.
//!
//! Security: a named pipe is created with the default DACL, which grants
//! the creating user (plus SYSTEM and administrators) — the same
//! "only this user" property the Unix socket gets from its 0600 mode and
//! its `SO_PEERCRED` check. There is no peer-credential call available
//! without `unsafe` FFI, so this module does not repeat the uid check;
//! `peer_ok` answers the DACL's guarantee.

use std::io;
use std::path::PathBuf;

use ka_agent_runtime::paths::Paths;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::windows::named_pipe::{
    ClientOptions, NamedPipeClient, NamedPipeServer, ServerOptions,
};

/// The pipe name for a data root (stable, per-root; derived in
/// [`super::pipe_name_for`] so it is testable on every platform).
pub fn socket_path(paths: &Paths) -> PathBuf {
    PathBuf::from(super::pipe_name_for(paths.root()))
}

/// The host side: one pipe instance, re-created after every accept (a
/// named-pipe instance serves exactly one client).
pub struct IpcListener {
    name: String,
    current: tokio::sync::Mutex<Option<NamedPipeServer>>,
}

impl IpcListener {
    /// Bind the first instance. `first_pipe_instance(true)` makes a second
    /// daemon on the same root fail here instead of stealing clients.
    pub fn bind(paths: &Paths) -> io::Result<Self> {
        let name = socket_path(paths).to_string_lossy().to_string();
        let server = ServerOptions::new()
            .first_pipe_instance(true)
            .create(&name)?;
        Ok(Self {
            name,
            current: tokio::sync::Mutex::new(Some(server)),
        })
    }

    pub async fn accept(&self) -> io::Result<IpcStream> {
        let server = {
            let mut slot = self.current.lock().await;
            match slot.take() {
                Some(server) => server,
                // The previous client was still connected: create the next
                // instance up front (the old one keeps its own).
                None => ServerOptions::new().create(&self.name)?,
            }
        };
        server.connect().await?;
        Ok(IpcStream {
            pipe: Pipe::Server(server),
        })
    }
}

/// Server and client handles are distinct tokio types; the stream carries
/// either and delegates the async traits.
enum Pipe {
    Server(NamedPipeServer),
    Client(NamedPipeClient),
}

/// One accepted or connected stream over the admin pipe.
pub struct IpcStream {
    pipe: Pipe,
}

impl IpcStream {
    /// The pipe's DACL already restricts it to the owner; see the module
    /// docs. Kept for signature parity with the Unix backend.
    pub fn peer_ok(&self) -> bool {
        true
    }

    /// Split into halves (the attach stream's reader/writer).
    pub fn split(self) -> (super::IpcRead, super::IpcWrite) {
        tokio::io::split(self)
    }
}

impl AsyncRead for IpcStream {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> std::task::Poll<io::Result<()>> {
        match &mut self.pipe {
            Pipe::Server(p) => std::pin::Pin::new(p).poll_read(cx, buf),
            Pipe::Client(p) => std::pin::Pin::new(p).poll_read(cx, buf),
        }
    }
}

impl AsyncWrite for IpcStream {
    fn poll_write(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<io::Result<usize>> {
        match &mut self.pipe {
            Pipe::Server(p) => std::pin::Pin::new(p).poll_write(cx, buf),
            Pipe::Client(p) => std::pin::Pin::new(p).poll_write(cx, buf),
        }
    }

    fn poll_flush(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<io::Result<()>> {
        match &mut self.pipe {
            Pipe::Server(p) => std::pin::Pin::new(p).poll_flush(cx),
            Pipe::Client(p) => std::pin::Pin::new(p).poll_flush(cx),
        }
    }

    fn poll_shutdown(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<io::Result<()>> {
        match &mut self.pipe {
            Pipe::Server(p) => std::pin::Pin::new(p).poll_shutdown(cx),
            Pipe::Client(p) => std::pin::Pin::new(p).poll_shutdown(cx),
        }
    }
}

/// Connect to the host's admin pipe (async), waiting while every instance
/// is busy (`ERROR_PIPE_BUSY` = 231).
pub async fn connect(paths: &Paths) -> io::Result<IpcStream> {
    let name = socket_path(paths).to_string_lossy().to_string();
    loop {
        match ClientOptions::new().open(&name) {
            Ok(pipe) => {
                return Ok(IpcStream {
                    pipe: Pipe::Client(pipe),
                });
            }
            Err(e) if e.raw_os_error() == Some(231) => {
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
            Err(e) => return Err(e),
        }
    }
}

/// A blocking admin stream: one request line out, one reply line in.
pub struct BlockingStream {
    file: std::fs::File,
}

impl BlockingStream {
    /// Pipes have no per-handle timeouts; the host closes a stream it
    /// stops serving, so a hung read ends with the host.
    pub fn set_read_timeout(&self, _t: Option<std::time::Duration>) -> io::Result<()> {
        Ok(())
    }

    pub fn set_write_timeout(&self, _t: Option<std::time::Duration>) -> io::Result<()> {
        Ok(())
    }
}

impl io::Read for BlockingStream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.file.read(buf)
    }
}

impl io::Write for BlockingStream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.file.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.file.flush()
    }
}

impl io::Read for &BlockingStream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        (&self.file).read(buf)
    }
}

impl io::Write for &BlockingStream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        (&self.file).write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        (&self.file).flush()
    }
}

/// Connect from blocking code (one-shot clients such as `ka-agent status`).
/// `OpenOptions` on a pipe path is the documented blocking client, and it
/// waits for an instance while the pipe is busy.
pub fn connect_blocking(paths: &Paths) -> io::Result<BlockingStream> {
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(socket_path(paths))?;
    Ok(BlockingStream { file })
}

/// Whether a host answers on the admin pipe right now.
pub fn is_running(paths: &Paths) -> bool {
    connect_blocking(paths).is_ok()
}
