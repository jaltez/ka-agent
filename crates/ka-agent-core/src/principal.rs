//! The acting principal (ADR-0004): the user a turn belongs to and the role
//! it acts with.
//!
//! Every surface resolves its source (`telegram:42`, `web:ana`, `console`,
//! `cron:task_x`, …) to a principal *before* it may submit anything. The
//! runtime refuses a source with no principal — exactly the refusal an
//! unknown sender gets — so an unlinked identity or a disabled account can
//! never reach a turn.

use crate::{ADMIN_ROLE, OPERATOR_USER};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Principal {
    pub username: String,
    /// The role the user holds (names a `[[role]]`; `admin` for the
    /// operator).
    pub role: String,
}

impl Principal {
    pub fn new(username: &str, role: &str) -> Self {
        Self {
            username: username.to_string(),
            role: role.to_string(),
        }
    }

    /// The operator: local surfaces (terminal, CLI, loopback web).
    pub fn operator() -> Self {
        Self::new(OPERATOR_USER, ADMIN_ROLE)
    }

    pub fn is_admin(&self) -> bool {
        self.role == ADMIN_ROLE
    }

    pub fn is_operator(&self) -> bool {
        self.username == OPERATOR_USER
    }

    /// A `(username, role)` pair from the store (`users` row or identity
    /// resolution).
    pub fn from_pair((username, role): (String, String)) -> Self {
        Self { username, role }
    }
}

impl std::fmt::Display for Principal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} ({})", self.username, self.role)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[test]
    fn operator_is_admin() {
        let p = Principal::operator();
        assert!(p.is_admin());
        assert!(p.is_operator());
        let member = Principal::new("ana", "member");
        assert!(!member.is_admin());
        assert!(!member.is_operator());
        assert_eq!(member.to_string(), "ana (member)");
    }
}
