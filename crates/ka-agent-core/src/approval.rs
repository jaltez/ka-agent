//! Approvals: a recorded human decision on one specific pending action
//! (SEED.md §2.11, §5.5, §8.3). Two kinds: Ka engine asks and Ka Agent
//! intents. Single-use, expiring, deny-on-expiry.

use serde::{Deserialize, Serialize};

use crate::ids::{ApprovalId, SessionId, TurnId};

/// What is being approved.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ApprovalKind {
    /// An ask raised by the Ka engine (`Event::Ask`): a permission ask
    /// (options `allow / always / deny`) or any other question the engine
    /// poses on the same event — its `continue / stop` session guards, a
    /// checkpoint confirmation, an MCP elicitation. [`is_permission_ask`]
    /// tells them apart.
    KaAsk {
        /// Ka's ask id (e.g. `ask-3`), needed to send `Command::Answer`.
        ask_id: String,
        question: String,
        /// Rendered detail (e.g. a diff), when the engine provided it.
        detail: Option<String>,
        /// Option labels, in engine order. A permission ask carries
        /// [`PERMISSION_ASK_OPTIONS`]; any other ask its own options,
        /// ending with the decline.
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
    /// Ka `Command::Answer` choice index for a permission ask (engine
    /// order: 0 = allow, 1 = always, 2 = deny). Any other ask is answered
    /// by its own options instead: [`Self::ka_choice_for`].
    pub fn ka_choice(self) -> usize {
        match self {
            ApprovalDecision::Allow => 0,
            ApprovalDecision::Always => 1,
            ApprovalDecision::Deny => 2,
        }
    }

    /// Ka `Command::Answer` choice index for a Ka ask whose options are
    /// `options`: the ask's shape decides.
    ///
    /// - A permission ask ([`is_permission_ask`]): [`Self::ka_choice`].
    /// - Any other ask, answered by its own options (the engine always
    ///   puts the decline last): `Allow` accepts the first option (index
    ///   0), `Deny` answers the decline (the last index), and `Always`
    ///   is a permission grant that is never offered on it (`None`).
    ///
    /// Also `None` when the ask offers no option to answer with. The
    /// engine takes an index as it comes, so a caller that gets `None`
    /// must not send one: it ends the cycle instead (fail closed).
    pub fn ka_choice_for(self, options: &[String]) -> Option<usize> {
        if is_permission_ask(options) {
            return Some(self.ka_choice());
        }
        match self {
            ApprovalDecision::Allow => (!options.is_empty()).then_some(0),
            ApprovalDecision::Deny => options.len().checked_sub(1),
            ApprovalDecision::Always => None,
        }
    }
}

/// The engine's fixed permission options, in the order its permission gate
/// poses them.
pub const PERMISSION_ASK_OPTIONS: [&str; 3] = ["allow", "always", "deny"];

/// Why `always`, allow-for-session and the durable rules are refused on an
/// ask that is not a permission ask.
pub const NOT_A_PERMISSION_ASK: &str = "that ask offers its own options — `always` and \
     allow-for-session apply to permission asks only; answer it with allow or deny";

/// Is this engine ask a permission ask? Exactly when its options are
/// [`PERMISSION_ASK_OPTIONS`]. A permission ask is the only kind that
/// grants anything, so it is the only kind `always`, allow-for-session and
/// the durable rules may answer; every other ask on `Event::Ask` (the
/// engine's `continue / stop` session guards, a checkpoint confirmation,
/// an MCP elicitation) carries its own options and is decided one card at
/// a time.
pub fn is_permission_ask(options: &[String]) -> bool {
    options
        .iter()
        .map(String::as_str)
        .eq(PERMISSION_ASK_OPTIONS)
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
    /// The user acting in the turn that raised this card; `None` = the
    /// admin queue's own card (pre-users rows and operator asks).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub username: Option<String>,
    /// Escalated to the admin queue (D9): the acting user may not decide
    /// it. `false` = self-scope, decided on the acting user's surfaces.
    #[serde(default)]
    pub escalated: bool,
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
            username: None,
            escalated: false,
        }
    }

    /// Route this card (D9): `username` acts, and `escalated` says whether
    /// the admin queue owns the decision (shared resource, over quota, or
    /// an `escalate` hit).
    pub fn routed_to(mut self, username: &str, escalated: bool) -> Self {
        self.username = Some(username.to_string());
        self.escalated = escalated;
        self
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

    fn words(options: &[&str]) -> Vec<String> {
        options.iter().map(|o| o.to_string()).collect()
    }

    #[test]
    fn ka_choice_mapping() {
        // The permission triple keeps its fixed mapping, whichever way it
        // is asked for.
        let triple = words(&["allow", "always", "deny"]);
        assert_eq!(ApprovalDecision::Allow.ka_choice(), 0);
        assert_eq!(ApprovalDecision::Always.ka_choice(), 1);
        assert_eq!(ApprovalDecision::Deny.ka_choice(), 2);
        assert_eq!(ApprovalDecision::Allow.ka_choice_for(&triple), Some(0));
        assert_eq!(ApprovalDecision::Always.ka_choice_for(&triple), Some(1));
        assert_eq!(ApprovalDecision::Deny.ka_choice_for(&triple), Some(2));
    }

    #[test]
    fn only_the_exact_triple_is_a_permission_ask() {
        assert!(is_permission_ask(&words(&["allow", "always", "deny"])));
        assert!(!is_permission_ask(&words(&["allow", "deny"])));
        assert!(!is_permission_ask(&words(&["allow", "always"])));
        assert!(
            !is_permission_ask(&words(&["allow", "always", "deny", "decline"])),
            "a permission-looking prefix in front of a decline is another ask"
        );
        assert!(!is_permission_ask(&words(&["allow", "always", "allow"])));
        assert!(!is_permission_ask(&words(&["Allow", "always", "deny"])));
        assert!(!is_permission_ask(&[]));
    }

    #[test]
    fn elicitation_shapes_answer_by_index() {
        // The four shapes an MCP elicitation arrives in, each ending with
        // the decline: allow accepts the first option, deny answers the
        // decline, and `always` is never an answer.
        let shapes: [&[&str]; 4] = [
            &["red", "green", "blue", "decline"],
            &["true", "false", "decline"],
            &["accept (empty value)", "decline"],
            &["accept", "decline"],
        ];
        for shape in shapes {
            let options = words(shape);
            assert_eq!(
                ApprovalDecision::Allow.ka_choice_for(&options),
                Some(0),
                "{shape:?}"
            );
            assert_eq!(
                ApprovalDecision::Deny.ka_choice_for(&options),
                Some(shape.len() - 1),
                "{shape:?}"
            );
            assert_eq!(
                ApprovalDecision::Always.ka_choice_for(&options),
                None,
                "{shape:?}"
            );
        }
    }

    #[test]
    fn the_engines_own_two_option_asks_decline_on_the_last_index() {
        for (options, decline) in [
            (words(&["continue", "stop"]), 1),
            (words(&["restore", "cancel"]), 1),
        ] {
            assert_eq!(ApprovalDecision::Allow.ka_choice_for(&options), Some(0));
            assert_eq!(
                ApprovalDecision::Deny.ka_choice_for(&options),
                Some(decline)
            );
        }
    }

    #[test]
    fn an_ask_without_options_is_never_answered_by_index() {
        // Index 0 would be read as "the first option" — a continue, a
        // restore, an accept — so nothing is sent for an empty ask.
        let none: Vec<String> = Vec::new();
        assert_eq!(ApprovalDecision::Allow.ka_choice_for(&none), None);
        assert_eq!(ApprovalDecision::Deny.ka_choice_for(&none), None);
        assert_eq!(ApprovalDecision::Always.ka_choice_for(&none), None);
    }
}
