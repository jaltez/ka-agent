//! Optional memory vector index (B1).
//!
//! Vectors are derived state. The Markdown memory tree remains the source
//! of truth; a rebuild replaces a user's rows from it. The store owns the
//! bytes and the per-user isolation; the runtime owns embedding, the
//! in-memory cache, and the scan.

use super::*;

/// One stored memory vector. `ref_id` is the accepted memory id.
#[derive(Debug, Clone)]
pub struct MemoryVectorRow {
    pub ref_id: String,
    pub scope: Option<String>,
    pub vec: Vec<f32>,
    pub updated_ms: i64,
}

fn encode(vec: &[f32]) -> Vec<u8> {
    let mut out = Vec::with_capacity(vec.len() * 4);
    for v in vec {
        out.extend_from_slice(&v.to_le_bytes());
    }
    out
}

fn decode(bytes: &[u8]) -> Option<Vec<f32>> {
    if bytes.is_empty() || !bytes.len().is_multiple_of(4) {
        return None;
    }
    let (chunks, _) = bytes.as_chunks::<4>();
    Some(chunks.iter().map(|c| f32::from_le_bytes(*c)).collect())
}

impl Store {
    /// Replace every vector row for one user in one transaction. The
    /// caller rebuilds the rows from the Markdown entries.
    pub fn replace_memory_vectors_for_user(
        &self,
        username: &str,
        model: &str,
        dim: usize,
        rows: &[MemoryVectorRow],
    ) -> Result<()> {
        if dim == 0 {
            return Err(StoreError::Refused(
                "memory vector dimension must be positive".into(),
            ));
        }
        for row in rows {
            if row.vec.len() != dim {
                return Err(StoreError::Refused(format!(
                    "memory vector {} has {} dimensions, expected {dim}",
                    row.ref_id,
                    row.vec.len()
                )));
            }
        }
        let mut conn = self.lock();
        let tx = conn.transaction()?;
        tx.execute(
            "DELETE FROM memory_vectors WHERE username = ?1",
            params![username],
        )?;
        {
            let mut stmt = tx.prepare(
                "INSERT INTO memory_vectors
                 (ref_id, username, scope, model, dim, vec, updated_ms)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            )?;
            for row in rows {
                stmt.execute(params![
                    &row.ref_id,
                    username,
                    row.scope.as_deref(),
                    model,
                    dim as i64,
                    encode(&row.vec),
                    row.updated_ms,
                ])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// Newest-first vectors for one user and model, capped by `limit`
    /// (the runtime's 50k per-user cap).
    pub fn memory_vectors_for_user(
        &self,
        username: &str,
        model: &str,
        limit: usize,
    ) -> Result<Vec<MemoryVectorRow>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT ref_id, scope, vec, updated_ms
             FROM memory_vectors
             WHERE username = ?1 AND model = ?2
             ORDER BY updated_ms DESC LIMIT ?3",
        )?;
        let rows = stmt.query_map(params![username, model, limit as i64], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, Option<String>>(1)?,
                r.get::<_, Vec<u8>>(2)?,
                r.get::<_, i64>(3)?,
            ))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (ref_id, scope, bytes, updated_ms) = row?;
            if let Some(vec) = decode(&bytes) {
                out.push(MemoryVectorRow {
                    ref_id,
                    scope,
                    vec,
                    updated_ms,
                });
            }
        }
        Ok(out)
    }

    /// Row count for one user, across models.
    pub fn count_memory_vectors_for_user(&self, username: &str) -> Result<usize> {
        let conn = self.lock();
        let count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM memory_vectors WHERE username = ?1",
            params![username],
            |r| r.get(0),
        )?;
        Ok(count.max(0) as usize)
    }

    /// Drop every vector for a user (forget, purge, model change).
    pub fn delete_memory_vectors_for_user(&self, username: &str) -> Result<()> {
        let conn = self.lock();
        conn.execute(
            "DELETE FROM memory_vectors WHERE username = ?1",
            params![username],
        )?;
        Ok(())
    }

    pub fn memory_index_meta(&self, key: &str) -> Result<Option<String>> {
        let conn = self.lock();
        conn.query_row(
            "SELECT value FROM memory_index_meta WHERE key = ?1",
            params![key],
            |r| r.get(0),
        )
        .optional()
        .map_err(StoreError::from)
    }

    pub fn set_memory_index_meta(&self, key: &str, value: &str) -> Result<()> {
        let conn = self.lock();
        conn.execute(
            "INSERT OR REPLACE INTO memory_index_meta(key, value) VALUES (?1, ?2)",
            params![key, value],
        )?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    fn row(id: &str, vec: Vec<f32>) -> MemoryVectorRow {
        MemoryVectorRow {
            ref_id: id.into(),
            scope: Some("project".into()),
            vec,
            updated_ms: 1_700_000_000_000,
        }
    }

    #[test]
    fn vectors_roundtrip_and_isolate_users() {
        let store = Store::open_in_memory().unwrap();
        store
            .replace_memory_vectors_for_user(
                "alice",
                "test-model",
                3,
                &[row("mem_a", vec![1.0, 0.0, 0.0])],
            )
            .unwrap();
        store
            .replace_memory_vectors_for_user(
                "bob",
                "test-model",
                3,
                &[row("mem_b", vec![0.0, 1.0, 0.0])],
            )
            .unwrap();
        let alice = store
            .memory_vectors_for_user("alice", "test-model", 50_000)
            .unwrap();
        assert_eq!(alice.len(), 1);
        assert_eq!(alice[0].ref_id, "mem_a");
        assert_eq!(alice[0].vec, vec![1.0, 0.0, 0.0]);
        assert_eq!(store.count_memory_vectors_for_user("bob").unwrap(), 1);
        store.delete_memory_vectors_for_user("alice").unwrap();
        assert_eq!(store.count_memory_vectors_for_user("alice").unwrap(), 0);
        assert_eq!(store.count_memory_vectors_for_user("bob").unwrap(), 1);
    }

    #[test]
    fn dimension_mismatch_is_refused() {
        let store = Store::open_in_memory().unwrap();
        let err = store
            .replace_memory_vectors_for_user(
                "alice",
                "test-model",
                3,
                &[row("mem_a", vec![1.0, 0.0])],
            )
            .unwrap_err();
        assert!(err.to_string().contains("dimensions"), "{err}");
    }
}
