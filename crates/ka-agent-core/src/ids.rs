//! Opaque string ids owned by Ka Agent. Ka's own ids (strand ids, ask ids)
//! stay opaque strings at this layer; they never masquerade as Ka Agent ids.

use serde::{Deserialize, Serialize};

/// One id value: `<prefix>_<hex time>-<hex counter>-<hex entropy>`.
/// Unique within a process by construction; unique across restarts with
/// overwhelming probability (time + per-process random entropy).
fn next_id() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static CTR: AtomicU64 = AtomicU64::new(0);
    let n = CTR.fetch_add(1, Ordering::Relaxed);
    let t = crate::now_ms().max(0) as u64;
    let entropy = {
        use std::hash::{BuildHasher, Hasher};
        let mut h = std::collections::hash_map::RandomState::new().build_hasher();
        h.write_u64(t);
        h.write_u64(n);
        h.finish()
    };
    format!("{t:x}-{n:02x}-{entropy:08x}")
}

macro_rules! string_id {
    ($name:ident, $prefix:literal) => {
        #[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
        pub struct $name(pub String);

        impl $name {
            /// Generate a fresh unique id.
            pub fn generate() -> Self {
                Self(format!(concat!($prefix, "_{}"), next_id()))
            }

            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(&self.0)
            }
        }
    };
}

string_id!(SessionId, "sess");
string_id!(TurnId, "turn");
string_id!(ApprovalId, "appr");
string_id!(QueueId, "q");
string_id!(TaskId, "task");
string_id!(RunId, "run");
string_id!(IntentId, "int");
string_id!(MemoryId, "mem");

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[test]
    fn ids_are_prefixed_and_unique() {
        let a = SessionId::generate();
        let b = SessionId::generate();
        assert!(a.0.starts_with("sess_"));
        assert_ne!(a, b);
        let t = TurnId::generate();
        assert!(t.0.starts_with("turn_"));
        assert!(t.0.len() > "turn_".len());
    }

    #[test]
    fn different_families_never_collide_lexically() {
        assert!(SessionId::generate().0.starts_with("sess_"));
        assert!(ApprovalId::generate().0.starts_with("appr_"));
    }
}
