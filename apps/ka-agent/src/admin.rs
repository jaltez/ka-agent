//! Client side of the host's admin socket (`<data>/admin.sock`): one-shot
//! JSON requests (`status`, `shutdown`, `reload`) and the long-lived
//! `attach` stream used by `ka-agent chat`.

use std::io::{BufRead, BufReader, Write};
use std::time::Duration;

use ka_agent_runtime::paths::Paths;
use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt};

use crate::ipc;

#[derive(Debug, thiserror::Error)]
pub enum AdminError {
    #[error("Ka Agent is not running (no admin socket at {0})")]
    NotRunning(String),
    #[error("admin socket: {0}")]
    Io(#[from] std::io::Error),
    #[error("the daemon closed the connection without a reply")]
    NoReply,
    #[error("bad reply from the daemon: {0}")]
    BadReply(String),
    #[error("{0}")]
    Daemon(String),
}

fn sock_path(paths: &Paths) -> std::path::PathBuf {
    ipc::socket_path(paths)
}

/// Send one request, read one reply line. A reply carrying `"error"`
/// becomes [`AdminError::Daemon`].
pub fn request(paths: &Paths, req: &serde_json::Value) -> Result<serde_json::Value, AdminError> {
    let sock = sock_path(paths);
    let mut stream = ipc::connect_blocking(paths).map_err(|e| {
        if matches!(
            e.kind(),
            std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
        ) {
            AdminError::NotRunning(sock.display().to_string())
        } else {
            AdminError::Io(e)
        }
    })?;
    stream.set_read_timeout(Some(Duration::from_secs(3)))?;
    stream.set_write_timeout(Some(Duration::from_secs(3)))?;
    stream.write_all(format!("{req}\n").as_bytes())?;
    stream.flush()?;
    let mut line = String::new();
    if BufReader::new(stream).read_line(&mut line)? == 0 {
        return Err(AdminError::NoReply);
    }
    let v: serde_json::Value =
        serde_json::from_str(line.trim()).map_err(|e| AdminError::BadReply(e.to_string()))?;
    if let Some(err) = v.get("error") {
        return Err(AdminError::Daemon(
            err.as_str().unwrap_or("error").to_string(),
        ));
    }
    Ok(v)
}

/// Whether a host (daemon or local chat) answers on the admin socket right
/// now.
pub fn is_running(paths: &Paths) -> bool {
    ipc::is_running(paths)
}

pub type AttachLines = tokio::io::Lines<tokio::io::BufReader<ipc::IpcRead>>;

/// Open an attach stream: the host answers with a `hello` event, then
/// every envelope; the client writes `{"op":"say","text":…}` lines.
/// `since` names the last seq this client saw (0 = live only).
pub async fn attach_since(
    paths: &Paths,
    since: u64,
) -> Result<(AttachLines, ipc::IpcWrite), AdminError> {
    let sock = sock_path(paths);
    let stream = ipc::connect(paths)
        .await
        .map_err(|_| AdminError::NotRunning(sock.display().to_string()))?;
    let (r, mut w) = stream.split();
    let hello = serde_json::json!({"op": "attach", "since": since});
    w.write_all(format!("{hello}\n").as_bytes()).await?;
    w.flush().await?;
    Ok((tokio::io::BufReader::new(r).lines(), w))
}

/// Live-only attach (the common chat client).
pub async fn attach(paths: &Paths) -> Result<(AttachLines, ipc::IpcWrite), AdminError> {
    attach_since(paths, 0).await
}
