//! Ka Agent domain types and state machines.
//!
//! This crate is the shared contract between every other crate: it has no I/O
//! and no Ka dependencies (SEED.md §11.2 — `ka-agent-core` has no I/O deps).
//! Everything here must be constructible and checkable in isolation; the
//! persistence layer (`ka-agent-store`) and the runtime (`ka-agent-runtime`)
//! build on these types.

pub mod approval;
pub mod audit;
pub mod budget;
pub mod config;
pub mod config_file;
pub mod ids;
pub mod layout;
pub mod markdown;
pub mod outbox;
pub mod queue;
pub mod secret;
pub mod session;
pub mod task;
pub mod time;
pub mod turn;
pub mod when;

pub use approval::{Approval, ApprovalDecision, ApprovalKind, ApprovalStatus};
pub use audit::AuditRecord;
pub use budget::{BudgetState, BudgetVerdict};
pub use config::{
    Calendar, DaemonConfig, Error as ConfigError, MatrixSettings, McpServer, Notify, Search, Voice,
    Web,
};
pub use ids::{ApprovalId, IntentId, MemoryId, QueueId, RunId, SessionId, TaskId, TurnId};
pub use outbox::{OutboxChunk, OutboxStatus, truncate_at_char};
pub use queue::QueueItem;
pub use session::{Session, SessionStatus};
pub use task::{CronMode, CronSpec, Task, TaskStatus};
pub use time::{fmt_interval, parse_duration};
pub use turn::{Turn, TurnStatus};
pub use when::When;

/// Milliseconds since the Unix epoch (UTC). All timestamps in Ka Agent state
/// are stored as this integer; rendering to a human format is a surface
/// concern.
pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Day index since the Unix epoch (UTC) — the budget window key.
pub fn day_key(now_ms: i64) -> i64 {
    now_ms.div_euclid(86_400_000)
}

/// Expand a leading `~` to `$HOME`. Untouched otherwise (relative paths stay
/// relative to the daemon's working directory).
pub fn expand_home(path: &str) -> std::path::PathBuf {
    if let Some(rest) = path.strip_prefix("~/")
        && let Some(home) = std::env::var_os("HOME")
    {
        return std::path::Path::new(&home).join(rest);
    }
    std::path::PathBuf::from(path)
}
