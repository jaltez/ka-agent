//! Ka Agent runtime: session management, durable queue, approvals, budgets
//! and the worker supervisor (SEED.md §3.2, §3.8). This crate glues the port
//! (Ka boundary), the store (state) and a channel-agnostic `Surface`.

pub mod briefing;
pub mod bus;
pub mod cron_ops;
pub mod event;
pub mod paths;
pub mod runtime;
pub mod skills;
pub mod status;
pub mod surface;

pub use bus::EventBus;
pub use event::{AgentEvent, Envelope, PlanItem};
pub use paths::{InstanceLock, LockError, Paths};
pub use runtime::{CancelOutcome, Error, ModelKeyState, ReloadReport, Runtime, memory_preamble};
pub use status::StatusReport;
pub use surface::{ApprovalCard, Surface};
