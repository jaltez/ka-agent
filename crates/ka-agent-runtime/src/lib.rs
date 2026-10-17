//! Ka Agent runtime: session management, durable queue, approvals, budgets
//! and the worker supervisor (SEED.md §3.2, §3.8). This crate glues the port
//! (Ka boundary), the store (state) and a channel-agnostic `Surface`.

pub mod paths;
pub mod runtime;
pub mod surface;

pub use paths::Paths;
pub use runtime::{Error, Runtime, next_daily_utc};
pub use surface::{ApprovalCard, Surface};
