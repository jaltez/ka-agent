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

/// The operator-facing handle of an id: its last 8 characters. The head of
/// an id is time-ordered (ids minted seconds apart share it); the tail is
/// the per-id entropy, so 8 characters of it are unique in practice.
pub fn short_id(id: &str) -> &str {
    let start = id.char_indices().rev().nth(7).map(|(i, _)| i).unwrap_or(0);
    &id[start..]
}

/// Whether `query` names `id`: the full id, a prefix (with or without the
/// family prefix such as `mem_`), or a suffix of at least 4 characters —
/// which is what [`short_id`] shows.
pub fn id_matches(id: &str, query: &str) -> bool {
    let q = query.trim();
    if q.is_empty() {
        return false;
    }
    if id == q || id.starts_with(q) {
        return true;
    }
    if let Some((_, body)) = id.split_once('_')
        && body.starts_with(q)
    {
        return true;
    }
    q.len() >= 4 && id.ends_with(q)
}

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

    #[test]
    fn short_ids_are_tails_and_match_back() {
        let id = "mem_19a3f2b4c1d-03-8f2c11aa4b7e90d1";
        assert_eq!(short_id(id), "4b7e90d1");
        assert!(id_matches(id, "4b7e90d1"));
        assert!(id_matches(id, "mem_19a3"));
        assert!(id_matches(id, "19a3f2"));
        assert!(!id_matches(id, "90d"));
        assert!(!id_matches(id, ""));
        assert_eq!(short_id("abc"), "abc");
    }
}
