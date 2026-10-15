//! Prompt cycles: one submitted user message through `idle`
//! (SEED.md §2.5, §3.7). Ka Agent's turn table is authoritative for
//! interruption status (gap G9 mitigation).

use serde::{Deserialize, Serialize};

use crate::ids::{SessionId, TurnId};

/// Why/whether a prompt cycle finished.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnStatus {
    /// Recorded in SQLite before submission (gap G8 mitigation).
    Queued,
    /// `prompt` handed to the port.
    Submitted,
    /// `turn_started` seen, `idle` not yet.
    Running,
    /// Finished with a final answer.
    Done,
    /// Finished with `stop: error`.
    Error,
    /// Finished with `stop: aborted` (user `/stop`).
    Aborted,
    /// The worker/engine died mid-cycle (channel closed without `idle`).
    Interrupted,
    /// Abort escalation exhausted: the worker was SIGKILLed.
    Killed,
}

impl TurnStatus {
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            TurnStatus::Done | TurnStatus::Error | TurnStatus::Aborted | TurnStatus::Interrupted | TurnStatus::Killed
        )
    }

    pub fn can_transition_to(self, next: Self) -> bool {
        use TurnStatus::*;
        matches!(
            (self, next),
            (Queued, Submitted)
                | (Queued, Interrupted) // daemon died before submission
                | (Submitted, Running)
                | (Submitted, Interrupted)
                | (Running, Done)
                | (Running, Error)
                | (Running, Aborted)
                | (Running, Interrupted)
                | (Running, Killed)
                | (Aborted, Killed)
        )
    }
}

/// One prompt cycle.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Turn {
    pub id: TurnId,
    pub session: SessionId,
    pub inbound_text: String,
    pub status: TurnStatus,
    /// Ka's `Stop` value when terminal (`done`, `error`, `aborted`, ...).
    pub stop: Option<String>,
    /// Ka's `ErrorClass` when the cycle failed.
    pub error_class: Option<String>,
    pub error_message: Option<String>,
    /// Turn cost in USD (ka's usage accounting; 0 for unpriced models).
    pub cost_usd: f64,
    /// input + output + cache tokens.
    pub tokens: u64,
    pub created_ms: i64,
    pub updated_ms: i64,
}

impl Turn {
    pub fn new(session: SessionId, inbound_text: String) -> Self {
        let now = crate::now_ms();
        Self {
            id: TurnId::generate(),
            session,
            inbound_text,
            status: TurnStatus::Queued,
            stop: None,
            error_class: None,
            error_message: None,
            cost_usd: 0.0,
            tokens: 0,
            created_ms: now,
            updated_ms: now,
        }
    }

    pub fn transition(&mut self, next: TurnStatus) -> Result<(), TurnStatus> {
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
    fn lifecycle() {
        let mut t = Turn::new(SessionId::generate(), "hi".into());
        t.transition(TurnStatus::Submitted).unwrap();
        t.transition(TurnStatus::Running).unwrap();
        t.transition(TurnStatus::Done).unwrap();
        assert!(t.status.is_terminal());
        assert!(t.transition(TurnStatus::Running).is_err());
    }

    #[test]
    fn interruption_paths() {
        // daemon crash before submission
        let mut a = Turn::new(SessionId::generate(), "x".into());
        a.transition(TurnStatus::Interrupted).unwrap();
        // engine death mid-cycle
        let mut b = Turn::new(SessionId::generate(), "x".into());
        b.transition(TurnStatus::Submitted).unwrap();
        b.transition(TurnStatus::Running).unwrap();
        b.transition(TurnStatus::Interrupted).unwrap();
        // kill escalation after abort
        let mut c = Turn::new(SessionId::generate(), "x".into());
        c.transition(TurnStatus::Submitted).unwrap();
        c.transition(TurnStatus::Running).unwrap();
        c.transition(TurnStatus::Aborted).unwrap();
        c.transition(TurnStatus::Killed).unwrap();
    }

    #[test]
    fn done_cannot_be_interrupted() {
        let mut t = Turn::new(SessionId::generate(), "x".into());
        t.transition(TurnStatus::Submitted).unwrap();
        t.transition(TurnStatus::Running).unwrap();
        t.transition(TurnStatus::Done).unwrap();
        assert!(t.transition(TurnStatus::Interrupted).is_err());
    }
}
