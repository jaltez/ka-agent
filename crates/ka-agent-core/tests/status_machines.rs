//! Golden edge matrices for the four status machines. `can_transition_to`
//! is load-bearing: the store refuses illegal writes and the runtime
//! builds its handlers around the legal set. These tests lock the set —
//! adding an edge now means updating the matrix consciously, not by
//! accident in a handler three files away.

use ka_agent_core::{ApprovalStatus, SessionStatus, TaskStatus, TurnStatus};

const SESSIONS: &[SessionStatus] = &[
    SessionStatus::Idle,
    SessionStatus::Running,
    SessionStatus::AwaitingApproval,
    SessionStatus::Stopping,
    SessionStatus::Paused,
    SessionStatus::Degraded,
];

const SESSION_EDGES: &[(SessionStatus, SessionStatus)] = &[
    (SessionStatus::Idle, SessionStatus::Running),
    (SessionStatus::Running, SessionStatus::AwaitingApproval),
    (SessionStatus::AwaitingApproval, SessionStatus::Running),
    (SessionStatus::Running, SessionStatus::Idle),
    (SessionStatus::AwaitingApproval, SessionStatus::Idle),
    (SessionStatus::Running, SessionStatus::Stopping),
    (SessionStatus::AwaitingApproval, SessionStatus::Stopping),
    (SessionStatus::Stopping, SessionStatus::Idle),
    (SessionStatus::Idle, SessionStatus::Paused),
    (SessionStatus::Running, SessionStatus::Paused),
    (SessionStatus::AwaitingApproval, SessionStatus::Paused),
    (SessionStatus::Stopping, SessionStatus::Paused),
    (SessionStatus::Paused, SessionStatus::Idle),
    (SessionStatus::Degraded, SessionStatus::Idle),
    (SessionStatus::Idle, SessionStatus::Degraded),
    (SessionStatus::Paused, SessionStatus::Degraded),
    (SessionStatus::Running, SessionStatus::Degraded),
    (SessionStatus::AwaitingApproval, SessionStatus::Degraded),
    (SessionStatus::Stopping, SessionStatus::Degraded),
    // restart reconciliation: an interrupted cycle re-submits from Degraded
    (SessionStatus::Degraded, SessionStatus::Running),
];

const TURNS: &[TurnStatus] = &[
    TurnStatus::Queued,
    TurnStatus::Submitted,
    TurnStatus::Running,
    TurnStatus::Done,
    TurnStatus::Error,
    TurnStatus::Aborted,
    TurnStatus::Interrupted,
    TurnStatus::Killed,
];

const TURN_EDGES: &[(TurnStatus, TurnStatus)] = &[
    (TurnStatus::Queued, TurnStatus::Submitted),
    (TurnStatus::Queued, TurnStatus::Interrupted), // daemon died before submission
    (TurnStatus::Submitted, TurnStatus::Running),
    (TurnStatus::Submitted, TurnStatus::Interrupted),
    (TurnStatus::Running, TurnStatus::Done),
    (TurnStatus::Running, TurnStatus::Error),
    (TurnStatus::Running, TurnStatus::Aborted),
    (TurnStatus::Running, TurnStatus::Interrupted),
    (TurnStatus::Running, TurnStatus::Killed),
    (TurnStatus::Aborted, TurnStatus::Killed),
];

const APPROVALS: &[ApprovalStatus] = &[
    ApprovalStatus::Pending,
    ApprovalStatus::Approved,
    ApprovalStatus::Denied,
    ApprovalStatus::Expired,
    ApprovalStatus::Cancelled,
    ApprovalStatus::Superseded,
];

const APPROVAL_EDGES: &[(ApprovalStatus, ApprovalStatus)] = &[
    (ApprovalStatus::Pending, ApprovalStatus::Approved),
    (ApprovalStatus::Pending, ApprovalStatus::Denied),
    (ApprovalStatus::Pending, ApprovalStatus::Expired),
    (ApprovalStatus::Pending, ApprovalStatus::Cancelled), // turn aborted/engine gone
    (ApprovalStatus::Pending, ApprovalStatus::Superseded), // newer ask replaced it
];

const TASKS: &[TaskStatus] = &[
    TaskStatus::Queued,
    TaskStatus::Running,
    TaskStatus::WaitingApproval,
    TaskStatus::Succeeded,
    TaskStatus::Failed,
    TaskStatus::Cancelled,
    TaskStatus::Interrupted,
    TaskStatus::Unknown,
    TaskStatus::Missed,
];

const TASK_EDGES: &[(TaskStatus, TaskStatus)] = &[
    (TaskStatus::Queued, TaskStatus::Running),
    (TaskStatus::Queued, TaskStatus::Cancelled),
    (TaskStatus::Running, TaskStatus::WaitingApproval),
    (TaskStatus::WaitingApproval, TaskStatus::Running),
    (TaskStatus::WaitingApproval, TaskStatus::Cancelled),
    (TaskStatus::Running, TaskStatus::Succeeded),
    (TaskStatus::Running, TaskStatus::Failed),
    (TaskStatus::Running, TaskStatus::Cancelled),
    (TaskStatus::Running, TaskStatus::Interrupted),
    (TaskStatus::WaitingApproval, TaskStatus::Interrupted),
    (TaskStatus::Running, TaskStatus::Unknown),
    (TaskStatus::WaitingApproval, TaskStatus::Unknown),
    // operator-driven reconciliation only
    (TaskStatus::Interrupted, TaskStatus::Queued),
    (TaskStatus::Unknown, TaskStatus::Queued),
    (TaskStatus::Interrupted, TaskStatus::Cancelled),
    (TaskStatus::Unknown, TaskStatus::Cancelled),
    (TaskStatus::Failed, TaskStatus::Queued),
    (TaskStatus::Running, TaskStatus::Missed),
];

fn assert_matrix<S>(states: &[S], edges: &[(S, S)], name: &str)
where
    S: Copy + PartialEq + std::fmt::Debug + CanTransitionValue<S>,
{
    for from in states {
        for to in states {
            let wanted = edges.contains(&(*from, *to));
            let got = from.can_transition_to_value(*to);
            assert_eq!(
                got, wanted,
                "{name}: {from:?} -> {to:?} should be {}",
                if wanted { "legal" } else { "illegal" }
            );
        }
    }
    // No edge names a state outside the machine.
    for (from, to) in edges {
        assert!(states.contains(from), "{name}: edge from unknown state {from:?}");
        assert!(states.contains(to), "{name}: edge to unknown state {to:?}");
    }
}

/// A tiny dispatch so the generic assert can call `can_transition_to`
/// without trait plumbing on the core types.
trait CanTransitionValue<S> {
    fn can_transition_to_value(self, next: S) -> bool;
}

impl CanTransitionValue<SessionStatus> for SessionStatus {
    fn can_transition_to_value(self, next: SessionStatus) -> bool {
        SessionStatus::can_transition_to(self, next)
    }
}

impl CanTransitionValue<TurnStatus> for TurnStatus {
    fn can_transition_to_value(self, next: TurnStatus) -> bool {
        TurnStatus::can_transition_to(self, next)
    }
}

impl CanTransitionValue<ApprovalStatus> for ApprovalStatus {
    fn can_transition_to_value(self, next: ApprovalStatus) -> bool {
        ApprovalStatus::can_transition_to(self, next)
    }
}

impl CanTransitionValue<TaskStatus> for TaskStatus {
    fn can_transition_to_value(self, next: TaskStatus) -> bool {
        TaskStatus::can_transition_to(self, next)
    }
}

#[test]
fn session_status_matrix() {
    assert_matrix(SESSIONS, SESSION_EDGES, "SessionStatus");
}

#[test]
fn turn_status_matrix() {
    assert_matrix(TURNS, TURN_EDGES, "TurnStatus");
}

#[test]
fn approval_status_matrix() {
    assert_matrix(APPROVALS, APPROVAL_EDGES, "ApprovalStatus");
    assert_eq!(
        APPROVALS
            .iter()
            .filter(|s| !s.is_terminal())
            .collect::<Vec<_>>(),
        vec![&ApprovalStatus::Pending],
        "only Pending is non-terminal"
    );
}

#[test]
fn task_status_matrix() {
    assert_matrix(TASKS, TASK_EDGES, "TaskStatus");
}
