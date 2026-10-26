//! Platform IPC for the runtime host: the admin endpoint every host serves
//! (daemon and local chat alike). Unix domain sockets today; the Windows
//! named-pipe swap (plan 4.2) replaces `unix.rs` beside this file.

mod server;

#[cfg(unix)]
mod unix;

#[cfg(unix)]
pub use unix::{IpcListener, IpcStream, connect, connect_blocking, is_running, socket_path};

pub(crate) use server::{AdminCtx, serve_admin};
