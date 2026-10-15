//! Tasks: units of work tracked by Ka Agent with a persisted state machine
//! (SEED.md §2.10, §7.2). The scheduler arrives in Phase 3; the machine and
//! the restart rules are fixed now.

use serde::{Deserialize, Serialize};

use crate::ids::{SessionId, TaskId};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    Queued,
    Running,
    WaitingApproval,
    Succeeded,
    Failed,
    Cancelled,
    /// Found `running` at startup after a crash.
    Interrupted,
    /// A side effect may or may not have happened; never auto-retried
    /// (SEED.md §7.3).
    Unknown,
}

impl TaskStatus {
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            TaskStatus::Succeeded
                | TaskStatus::Failed
                | TaskStatus::Cancelled
                | TaskStatus::Interrupted
                | TaskStatus::Unknown
        )
    }

    pub fn can_transition_to(self, next: Self) -> bool {
        use TaskStatus::*;
        matches!(
            (self, next),
            (Queued, Running)
                | (Queued, Cancelled)
                | (Running, WaitingApproval)
                | (WaitingApproval, Running)
                | (WaitingApproval, Cancelled)
                | (Running, Succeeded)
                | (Running, Failed)
                | (Running, Cancelled)
                | (Running, Interrupted)
                | (WaitingApproval, Interrupted)
                | (Running, Unknown)
                | (WaitingApproval, Unknown)
                // operator-driven reconciliation only
                | (Interrupted, Queued)
                | (Unknown, Queued)
                | (Interrupted, Cancelled)
                | (Unknown, Cancelled)
                | (Failed, Queued)
        )
    }

    /// Restart reconciliation (SEED.md §7.2): `running` becomes interrupted;
    /// approvals for intents survive (the action never executed).
    pub fn on_restart(self) -> Self {
        match self {
            TaskStatus::Running => TaskStatus::Interrupted,
            other => other,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Task {
    pub id: TaskId,
    pub session: Option<SessionId>,
    pub title: String,
    pub kind: String,
    pub status: TaskStatus,
    /// True for read-only kinds, which may be re-queued automatically once.
    pub read_only: bool,
    pub scheduled_at_ms: Option<i64>,
    pub created_ms: i64,
    pub updated_ms: i64,
}

impl Task {
    pub fn new(title: String, kind: &str, read_only: bool) -> Self {
        let now = crate::now_ms();
        Self {
            id: TaskId::generate(),
            session: None,
            title,
            kind: kind.to_string(),
            status: TaskStatus::Queued,
            read_only,
            scheduled_at_ms: None,
            created_ms: now,
            updated_ms: now,
        }
    }

    pub fn transition(&mut self, next: TaskStatus) -> Result<(), TaskStatus> {
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
    fn restart_reconciles_running_only() {
        assert_eq!(TaskStatus::Running.on_restart(), TaskStatus::Interrupted);
        assert_eq!(TaskStatus::Queued.on_restart(), TaskStatus::Queued);
        assert_eq!(
            TaskStatus::WaitingApproval.on_restart(),
            TaskStatus::WaitingApproval
        );
    }

    #[test]
    fn unknown_never_auto_reruns() {
        let mut t = Task::new("digest".into(), "rss", true);
        t.transition(TaskStatus::Running).unwrap();
        t.transition(TaskStatus::Unknown).unwrap();
        // only the operator can re-queue
        assert!(t.transition(TaskStatus::Running).is_err());
        t.transition(TaskStatus::Queued).unwrap();
    }
}
