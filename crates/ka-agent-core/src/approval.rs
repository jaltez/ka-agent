//! Approvals: a recorded human decision on one specific pending action
//! (SEED.md §2.11, §5.5, §8.3). Two kinds: Ka engine asks and Ka Agent
//! intents. Single-use, expiring, deny-on-expiry.

use serde::{Deserialize, Serialize};

use crate::ids::{ApprovalId, SessionId, TurnId};

/// What is being approved.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ApprovalKind {
    /// A permission ask raised by the Ka engine (`Event::Ask`).
    KaAsk {
        /// Ka's ask id (e.g. `ask-3`), needed to send `Command::Answer`.
        ask_id: String,
        question: String,
        /// Rendered detail (e.g. a diff), when the engine provided it.
        detail: Option<String>,
        /// Option labels, in engine order (allow / always / deny).
        options: Vec<String>,
    },
    /// A side-effecting action proposed through a Ka Agent connector.
    /// (Phase 3; the state machine is defined now so the store schema is
    /// stable.)
    Intent {
        tool: String,
        args_summary: String,
        effect: String,
    },
}

/// Approval state (SEED.md §5.5).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalStatus {
    Pending,
    Approved,
    Denied,
    Expired,
    Cancelled,
    Superseded,
}

impl ApprovalStatus {
    pub fn is_terminal(self) -> bool {
        !matches!(self, ApprovalStatus::Pending)
    }

    pub fn can_transition_to(self, next: Self) -> bool {
        use ApprovalStatus::*;
        matches!(
            (self, next),
            (Pending, Approved)
                | (Pending, Denied)
                | (Pending, Expired)
                | (Pending, Cancelled) // turn aborted/engine gone
                | (Pending, Superseded) // a newer ask replaced it
        )
    }
}

/// The decision applied to a pending approval.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalDecision {
    Allow,
    Deny,
    /// Never offered from remote channels in the MVP (gap G12): "always"
    /// grants live in Ka Agent's own rules layer, edited from the local CLI.
    Always,
}

impl ApprovalDecision {
    /// Ka `Command::Answer` choice index for Ka asks (engine order:
    /// 0 = allow, 1 = always, 2 = deny).
    pub fn ka_choice(self) -> usize {
        match self {
            ApprovalDecision::Allow => 0,
            ApprovalDecision::Always => 1,
            ApprovalDecision::Deny => 2,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Approval {
    pub id: ApprovalId,
    pub session: SessionId,
    pub turn: TurnId,
    pub kind: ApprovalKind,
    pub status: ApprovalStatus,
    /// Expiry (epoch ms). On expiry the runtime answers deny (Ka asks) or
    /// drops the intent.
    pub expires_at_ms: i64,
    pub decided_at_ms: Option<i64>,
    pub created_ms: i64,
}

impl Approval {
    pub fn new(session: SessionId, turn: TurnId, kind: ApprovalKind, ttl_secs: i64) -> Self {
        let now = crate::now_ms();
        Self {
            id: ApprovalId::generate(),
            session,
            turn,
            kind,
            status: ApprovalStatus::Pending,
            expires_at_ms: now + ttl_secs * 1000,
            decided_at_ms: None,
            created_ms: now,
        }
    }

    pub fn is_expired(&self, now_ms: i64) -> bool {
        self.status == ApprovalStatus::Pending && now_ms >= self.expires_at_ms
    }

    pub fn decide(&mut self, status: ApprovalStatus, at_ms: i64) -> Result<(), ApprovalStatus> {
        if self.status.can_transition_to(status) {
            self.status = status;
            self.decided_at_ms = Some(at_ms);
            Ok(())
        } else {
            Err(status)
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    fn ka_ask() -> ApprovalKind {
        ApprovalKind::KaAsk {
            ask_id: "ask-1".into(),
            question: "run `cargo test`?".into(),
            detail: None,
            options: vec!["allow".into(), "always".into(), "deny".into()],
        }
    }

    #[test]
    fn pending_transitions_once() {
        let mut a = Approval::new(SessionId::generate(), TurnId::generate(), ka_ask(), 900);
        assert!(!a.is_expired(crate::now_ms()));
        a.decide(ApprovalStatus::Approved, crate::now_ms()).unwrap();
        assert!(a.status.is_terminal());
        assert!(a.decide(ApprovalStatus::Denied, crate::now_ms()).is_err());
    }

    #[test]
    fn expiry_only_while_pending() {
        let mut a = Approval::new(SessionId::generate(), TurnId::generate(), ka_ask(), 0);
        assert!(a.is_expired(crate::now_ms() + 10));
        a.decide(ApprovalStatus::Denied, crate::now_ms()).unwrap();
        assert!(!a.is_expired(crate::now_ms() + 10));
    }

    #[test]
    fn ka_choice_mapping() {
        assert_eq!(ApprovalDecision::Allow.ka_choice(), 0);
        assert_eq!(ApprovalDecision::Always.ka_choice(), 1);
        assert_eq!(ApprovalDecision::Deny.ka_choice(), 2);
    }
}
