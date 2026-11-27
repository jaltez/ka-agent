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
//! - [`strand`] — strand files read from the daemon side (the HTML
//!   export, the retention sweep), over `ka_strand` without an engine.

pub mod catalog;
pub mod engine_layer;
pub mod port;
pub mod proto;
pub mod registry;
pub mod scenario;
pub mod strand;
pub mod token_lock;
pub mod worker;

pub use port::{
    BoxFut, GuardGrants, KaPort, OpenRequest, PortError, PortEvent, PortSession,
    SIDE_ANSWER_UNAVAILABLE, StrandSelector, WorkerEnv, WorkerPort, kill_process_tree,
};
pub use proto::{ImagePayload, StrandSel, WorkerCommand, WorkerEvent};
pub use scenario::{Scenario, ScenarioSpeaker, Step};
pub use strand::{StrandGc, export_html, gc_strands};
pub use worker::{WorkerRun, run_worker, spill_path};

/// The ka-harness content this build is pinned to (`Cargo.toml`):
/// `v0.12.2` rev `f41ecf9` — the v0.7.0+ line (meta/lazy mcp hands
/// registered once, ka-harness#1; strand power-loss durability;
/// leaf-merged dialect overlays; landlock /dev surface; the
/// `ask|accept_edits|allow|readonly` permission language) plus hooks
/// v2, the embedded models.dev catalog with a runtime layer,
/// `[features] allow`, `session_diff` / `context_breakdown` /
/// `side_question`, idle recaps, and child voices inheriting the
/// parent's posture.
/// `doctor` reports it and backups record it;
/// `scripts/check-ka-deps.sh` verifies it matches the manifest.
pub const KA_PIN: &str = "f41ecf9";
