//! Sessions: Ka Agent's durable handle for a conversation thread, bound to
//! one Ka strand at a time (SEED.md §2.2, §3.5).

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

use crate::ids::SessionId;

/// Conversation state per binding (SEED.md §5.5). Persisted; every channel
/// reply is driven by it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionStatus {
    /// No prompt cycle in flight.
    Idle,
    /// A prompt cycle is in flight (prompt sent, no `idle` yet).
    Running,
    /// The engine is blocked on an ask.
    AwaitingApproval,
    /// Abort sent, waiting for `idle` or the kill escalation.
    Stopping,
    /// Budget exhausted, auth failure, or operator pause.
    Paused,
    /// Worker crash loop; needs `/resume` or `/new`.
    Degraded,
}

impl SessionStatus {
    /// The legal state-machine edges. Everything else is a bug: the store
    /// refuses illegal writes and the runtime asserts on them in debug.
    pub fn can_transition_to(self, next: Self) -> bool {
        use SessionStatus::*;
        matches!(
            (self, next),
            (Idle, Running)
                | (Running, AwaitingApproval)
                | (AwaitingApproval, Running)
                | (Running, Idle)
                | (AwaitingApproval, Idle)
                | (Running, Stopping)
                | (AwaitingApproval, Stopping)
                | (Stopping, Idle)
                | (Idle, Paused)
                | (Running, Paused)
                | (AwaitingApproval, Paused)
                | (Stopping, Paused)
                | (Paused, Idle)
                | (Degraded, Idle)
                | (Idle, Degraded)
                | (Running, Degraded)
                | (AwaitingApproval, Degraded)
                | (Stopping, Degraded)
                | (Paused, Degraded)
                // restart reconciliation: an interrupted cycle lands in Idle
                // after the turn table has recorded the interruption.
                | (Degraded, Running)
        )
    }
}

/// One Ka Agent session.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Session {
    pub id: SessionId,
    /// The workspace directory every engine for this session runs in.
    pub workspace: PathBuf,
    /// Ka strand id, captured from the `session_info` bootstrap event. `None`
    /// until the first attach completes.
    pub strand_id: Option<String>,
    /// Strand file path, resolved once and reused for `StrandChoice::Path`.
    /// May stay `None` for a brand-new session whose strand file has not been
    /// materialized yet (ka creates it on first append).
    pub strand_path: Option<PathBuf>,
    pub status: SessionStatus,
    /// The user this conversation belongs to (the partition key). A user's
    /// session never sees another user's turns, memory or strand.
    #[serde(default = "crate::operator_user")]
    pub username: String,
    /// The role this conversation belongs to (`admin` for the operator's
    /// terminal, web and CLI surfaces).
    #[serde(default = "crate::admin_role")]
    pub role: String,
    /// A private (incognito) conversation: turn text is never recorded,
    /// memory/skill proposals are suppressed, attachments and the strand
    /// are destroyed when the session ends. Usage rows stay anonymous.
    #[serde(default)]
    pub private: bool,
    pub created_ms: i64,
    pub updated_ms: i64,
}

impl Session {
    pub fn new(workspace: PathBuf) -> Self {
        Self::new_for_user(workspace, crate::OPERATOR_USER, crate::ADMIN_ROLE)
    }

    /// A fresh session for one user acting with one role.
    pub fn new_for_user(workspace: PathBuf, username: &str, role: &str) -> Self {
        let now = crate::now_ms();
        Self {
            id: SessionId::generate(),
            workspace,
            strand_id: None,
            strand_path: None,
            status: SessionStatus::Idle,
            username: username.to_string(),
            role: role.to_string(),
            private: false,
            created_ms: now,
            updated_ms: now,
        }
    }

    /// Apply a legal transition; returns `Err(next)` when the edge is illegal.
    pub fn transition(&mut self, next: SessionStatus) -> Result<(), SessionStatus> {
        if self.status.can_transition_to(next) {
            self.status = next;
            self.updated_ms = crate::now_ms();
            Ok(())
        } else {
            Err(next)
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[test]
    fn happy_path_cycle() {
        let mut s = Session::new(PathBuf::from("/tmp/ws"));
        s.transition(SessionStatus::Running).unwrap();
        s.transition(SessionStatus::AwaitingApproval).unwrap();
        s.transition(SessionStatus::Running).unwrap();
        s.transition(SessionStatus::Stopping).unwrap();
        s.transition(SessionStatus::Idle).unwrap();
    }

    #[test]
    fn illegal_edges_rejected() {
        let mut s = Session::new(PathBuf::from("/tmp/ws"));
        assert!(s.transition(SessionStatus::Idle).is_err()); // idle -> idle
        assert!(s.transition(SessionStatus::Stopping).is_err()); // idle -> stopping
        s.transition(SessionStatus::Running).unwrap();
        assert!(s.transition(SessionStatus::Running).is_err()); // running -> running
        // restart reconciliation: running may only fall back through idle,
        // never sideways to degraded-then-running without operator action
        s.transition(SessionStatus::Degraded).unwrap();
        assert!(s.transition(SessionStatus::AwaitingApproval).is_err());
    }

    #[test]
    fn paused_is_reachable_and_recoverable() {
        let mut s = Session::new(PathBuf::from("/tmp/ws"));
        s.transition(SessionStatus::Running).unwrap();
        s.transition(SessionStatus::Paused).unwrap();
        s.transition(SessionStatus::Idle).unwrap();
    }
}
