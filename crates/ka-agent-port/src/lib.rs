//! The stable Ka boundary (SEED.md §3.4, `docs/contract/ka-port.md`).
//!
//! This is the only crate that knows ka types. Above it, everything is
//! written against [`KaPort`] / [`PortEvent`]; below it, [`worker`] is
//! the code that runs inside a worker process (`ka-agent __worker`),
//! linking the pinned Ka crates and hosting one engine per session over
//! the NDJSON envelope in [`proto`] (`docs/contract/worker-protocol.md`).
//!
//! - [`port`] — the boundary trait and the subprocess-worker client.
//! - [`proto`] — the daemon ↔ worker wire envelope.
//! - [`worker`] — the worker-side engine host.
//! - [`scenario`] — the scripted speaker for deterministic, network-free
//!   tests and e2e runs.

pub mod catalog;
pub mod engine_layer;
pub mod port;
pub mod proto;
pub mod scenario;
pub mod token_lock;
pub mod worker;

pub use port::{
    BoxFut, KaPort, OpenRequest, PortError, PortEvent, PortSession, StrandSelector, WorkerEnv,
    WorkerPort,
};
pub use proto::{ImagePayload, StrandSel, WorkerCommand, WorkerEvent};
pub use scenario::{Scenario, ScenarioSpeaker, Step};
pub use worker::{WorkerRun, run_worker};

/// The ka-harness content this build is pinned to (`Cargo.toml`): the
/// `v0.4.1` rev `193ad5d` — identity_file, live steers, anthropic
/// user-merge, the embedder trampoline, native opencode-go. `doctor`
/// reports it and backups record it; `scripts/check-ka-deps.sh`
/// verifies it matches the manifest.
pub const KA_PIN: &str = "193ad5d";
