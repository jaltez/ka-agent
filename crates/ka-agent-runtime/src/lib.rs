//! Ka Agent runtime: session management, durable queue, approvals, budgets
//! and the worker supervisor (SEED.md §3.2, §3.8). This crate glues the port
//! (Ka boundary), the store (state) and a channel-agnostic `Surface`.

pub mod briefing;
pub mod bus;
pub mod cron_ops;
pub mod event;
pub mod hooks;
pub mod paths;
pub mod runtime;
pub mod skills;
pub mod status;
pub mod surface;

pub use bus::EventBus;
pub use event::{AgentEvent, Envelope, PlanItem};
/// The model catalog and the models.dev registry, re-exported so the
/// channels crate reaches them without a direct ka-agent-port edge.
pub use ka_agent_port::{catalog, registry};
pub use paths::{InstanceLock, LockError, Paths};
pub use runtime::compile_identity;
pub use runtime::{CancelOutcome, Error, ModelKeyState, ReloadReport, Runtime, memory_preamble};
pub use status::StatusReport;
pub use surface::{ApprovalCard, Surface};
