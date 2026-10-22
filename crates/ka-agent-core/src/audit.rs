//! Audit records (SEED.md §8.7). Append-only; the hash chain is computed by
//! the store, which owns serialization and hashing. This type stays pure
//! data.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuditRecord {
    /// 1-based sequence, assigned by the store.
    pub seq: i64,
    pub at_ms: i64,
    /// `operator`, `telegram:<user_id>`, `system`, `runtime`.
    pub actor: String,
    /// Dotted action name, e.g. `approval.decide`, `session.new`,
    /// `memory.accept`, `worker.restart`.
    pub action: String,
    /// Free-form subject (ids, tool names). Never secrets.
    pub subject: String,
    pub detail: Option<String>,
    /// Hash chain: `hash = sha256(prev_hash || canonical-json(record))`.
    /// Computed by the store; empty until persisted.
    pub prev_hash: String,
    pub hash: String,
}

impl AuditRecord {
    /// New pending record; seq/prev_hash/hash are filled by the store.
    pub fn new(actor: &str, action: &str, subject: &str, detail: Option<String>) -> Self {
        Self {
            seq: 0,
            at_ms: crate::now_ms(),
            actor: actor.to_string(),
            action: action.to_string(),
            subject: subject.to_string(),
            detail,
            prev_hash: String::new(),
            hash: String::new(),
        }
    }
}

/// Actions worth auditing, as constants to avoid string drift.
pub mod actions {
    pub const SESSION_NEW: &str = "session.new";
    pub const SESSION_RESUME: &str = "session.resume";
    pub const MODE_SET: &str = "mode.set";
    pub const APPROVAL_DECIDE: &str = "approval.decide";
    pub const APPROVAL_EXPIRE: &str = "approval.expire";
    pub const WORKER_RESTART: &str = "worker.restart";
    pub const MEMORY_ACCEPT: &str = "memory.accept";
    pub const MEMORY_EDIT: &str = "memory.edit";
    pub const BUDGET_PAUSE: &str = "budget.pause";
    pub const IDENTITY_LINK: &str = "identity.link";
    pub const IDENTITY_UNLINK: &str = "identity.unlink";
    pub const IDENTITY_PAIR: &str = "identity.pair";
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    #[test]
    fn new_record_is_pending_hash_material() {
        let r =
            super::AuditRecord::new("operator", super::actions::APPROVAL_DECIDE, "appr_x", None);
        assert_eq!(r.seq, 0);
        assert_eq!(r.hash, "");
        assert_eq!(r.action, "approval.decide");
    }
}
