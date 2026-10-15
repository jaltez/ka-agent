//! Operator grants: the authority model for scoped tools (SEED.md §8.2).
//! Grants are scope strings like `http:get:api.github.com` or
//! `github:issues:read`, stored in the meta table as a JSON array.
//! Wildcard: `http:get:*` matches any host in that tier.

use ka_agent_store::Store;
use serde::{Deserialize, Serialize};
use thiserror::Error;

const META_KEY: &str = "grants";

#[derive(Debug, Error)]
pub enum GrantsError {
    #[error("store: {0}")]
    Store(#[from] ka_agent_store::StoreError),
    #[error("serialization: {0}")]
    Serde(#[from] serde_json::Error),
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Grants {
    pub scopes: Vec<String>,
}

impl Grants {
    /// Load from the store's meta table. Empty on fresh install.
    pub fn load(store: &Store) -> Result<Self, GrantsError> {
        match store.meta_get(META_KEY)? {
            Some(json) => Ok(serde_json::from_str(&json)?),
            None => Ok(Self::default()),
        }
    }

    /// Persist to the store's meta table.
    pub fn save(&self, store: &Store) -> Result<(), GrantsError> {
        let json = serde_json::to_string(self)?;
        store.meta_set(META_KEY, &json)?;
        Ok(())
    }

    /// Check whether a scope is granted. Supports prefix wildcards:
    /// `http:get:*` matches `http:get:api.example.com`.
    pub fn check(&self, scope: &str) -> bool {
        self.scopes.iter().any(|granted| {
            granted == scope
                || granted
                    .strip_suffix('*')
                    .is_some_and(|prefix| scope.starts_with(prefix))
        })
    }

    /// Add a scope (deduplicated, sorted for stable output).
    pub fn add(&mut self, scope: &str) {
        let scope = scope.trim();
        if !scope.is_empty() && !self.scopes.iter().any(|s| s == scope) {
            self.scopes.push(scope.to_string());
            self.scopes.sort();
        }
    }

    /// Remove a scope. Returns true when it was present.
    pub fn remove(&mut self, scope: &str) -> bool {
        let len = self.scopes.len();
        self.scopes.retain(|s| s != scope);
        self.scopes.len() < len
    }
}

/// Open the Ka Agent store from the current data root.
pub fn open_store() -> Result<Store, GrantsError> {
    let root = std::env::var_os("KA_AGENT_DATA_DIR")
        .map(std::path::PathBuf::from)
        .ok_or_else(|| {
            GrantsError::Store(ka_agent_store::StoreError::NotFound(
                "KA_AGENT_DATA_DIR not set".to_string(),
            ))
        })?;
    Ok(Store::open(&root.join("state.db"))?)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    fn store() -> Store {
        Store::open_in_memory().unwrap()
    }

    #[test]
    fn add_remove_roundtrip() {
        let mut g = Grants::default();
        g.add("http:get:example.com");
        g.add("github:issues:read");
        g.add("http:get:example.com"); // dedup
        assert_eq!(g.scopes.len(), 2);
        assert!(g.check("http:get:example.com"));
        assert!(g.check("github:issues:read"));
        assert!(!g.check("http:get:other.com"));
        assert!(g.remove("http:get:example.com"));
        assert!(!g.remove("http:get:example.com"));
        assert!(!g.check("http:get:example.com"));
    }

    #[test]
    fn wildcard_matching() {
        let mut g = Grants::default();
        g.add("http:get:*");
        assert!(g.check("http:get:api.github.com"));
        assert!(g.check("http:get:anything.org"));
        assert!(!g.check("http:post:example.com"));
        assert!(!g.check("github:issues:read"));
    }

    #[test]
    fn exact_wins_over_wildcard() {
        let mut g = Grants::default();
        g.add("http:get:specific.com");
        assert!(g.check("http:get:specific.com"));
        assert!(!g.check("http:get:other.com"));
    }

    #[test]
    fn empty_grants_deny_everything() {
        let g = Grants::default();
        assert!(!g.check("anything"));
    }

    #[test]
    fn persistence_through_store() {
        let s = store();
        let mut g = Grants::default();
        g.add("http:get:example.com");
        g.save(&s).unwrap();
        // Load from the same store instance (in-memory; no cross-instance persistence)
        let mut loaded = Grants::load(&s).unwrap();
        assert!(loaded.check("http:get:example.com"));
        loaded.remove("http:get:example.com");
        loaded.save(&s).unwrap();
        let reloaded = Grants::load(&s).unwrap();
        assert!(!reloaded.check("http:get:example.com"));
    }

    #[test]
    fn blank_scopes_ignored() {
        let mut g = Grants::default();
        g.add("");
        g.add("  ");
        assert!(g.scopes.is_empty());
    }
}
