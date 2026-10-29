//! The Ka boundary: the [`KaPort`] trait and the subprocess-worker
//! client (`docs/contract/ka-port.md`).
//!
//! Everything above this crate is written against [`KaPort`] /
//! [`PortEvent`]; no ka type crosses the boundary (SEED §3.1.5). The
//! concrete implementation spawns the worker binary over NDJSON stdio
//! (`docs/contract/worker-protocol.md`), one worker per open session,
//! each in its own process group with a sanitized environment.
//!
//! Split from the former single `port.rs` (moves only): [`env`] holds
//! the worker environment and open request, [`events`] the event
//! projection, [`session`] the spawned-worker session.

mod env;
mod events;
mod session;

pub use env::{GuardGrants, OpenRequest, StrandSelector, WorkerEnv};
pub use events::PortEvent;
pub use session::PortSession;

use std::path::PathBuf;

/// A boxed, Send future — the return shape of [`KaPort::open`] so the
/// trait stays dyn-compatible.
pub type BoxFut<'a, T> = std::pin::Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Port-level failures.
#[derive(Debug, thiserror::Error)]
pub enum PortError {
    /// The worker binary could not be spawned.
    #[error("failed to spawn worker `{}`: {source}", binary.display())]
    Spawn {
        /// The binary that failed.
        binary: PathBuf,
        /// The underlying spawn error.
        source: std::io::Error,
    },
    /// Worker or runtime I/O failure.
    #[error("worker i/o: {0}")]
    Io(#[from] std::io::Error),
    /// The worker violated the protocol (bad line, no `ready`, …).
    #[error("worker protocol: {0}")]
    Protocol(String),
    /// The session was closed by this side; no further sends.
    #[error("port session closed")]
    Closed,
    /// The worker or its engine died; no further sends.
    #[error("engine closed")]
    EngineClosed,
}

/// The stable Ka boundary. Everything above the port crate depends on
/// this trait only; swapping the boundary implementation is a one-crate
/// change (SEED §3.4).
pub trait KaPort: Send + Sync {
    /// Start or resume one engine on one strand. Returns the live
    /// session handle with its event stream.
    fn open<'a>(&'a self, req: OpenRequest) -> BoxFut<'a, Result<PortSession, PortError>>;
}

/// The production port: spawn the worker binary per open.
#[derive(Debug, Clone)]
pub struct WorkerPort {
    /// Default worker binary (used when an [`OpenRequest`] names none).
    pub binary: PathBuf,
    /// When set, forced into every open's `scenario` field (console chat
    /// e2e; `None` in production).
    pub scenario_override: Option<PathBuf>,
}

impl WorkerPort {
    /// A port defaulting to the current executable (the daemon binary
    /// hosting `__worker` subcommands).
    pub fn with_current_exe() -> Self {
        Self {
            binary: std::env::current_exe().unwrap_or_else(|_| PathBuf::from("ka-agent")),
            scenario_override: None,
        }
    }
}

impl Default for WorkerPort {
    fn default() -> Self {
        Self {
            binary: PathBuf::new(),
            scenario_override: None,
        }
    }
}

impl KaPort for WorkerPort {
    fn open<'a>(&'a self, mut req: OpenRequest) -> BoxFut<'a, Result<PortSession, PortError>> {
        Box::pin(async move {
            if req.binary.as_os_str().is_empty() {
                req.binary = self.binary.clone();
            }
            if let Some(scenario) = &self.scenario_override {
                req.scenario = Some(scenario.clone());
            }
            PortSession::spawn(req).await
        })
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    #[test]
    fn with_current_exe_points_at_the_running_binary() {
        let port = WorkerPort::with_current_exe();
        assert_eq!(port.binary, std::env::current_exe().unwrap());
        assert!(port.scenario_override.is_none());
    }
}
