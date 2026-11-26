//! Optional memory vector index and the exact scan (B1).
//!
//! The index is per user and derived from the accepted Markdown bullets.
//! `MEMORY_VECTOR_CAP` bounds the rows the daemon scans; the runtime keeps
//! the current set in memory after the first query.

use std::sync::Arc;

use ka_agent_core::Principal;

use super::memory_embed::{CachedVectors, Embedder};
use super::*;

/// The per-user vector cap. 50k f32 vectors at 1024 dimensions is about
/// 200 MB in the cache; accepted memory should stay far below this.
pub const MEMORY_VECTOR_CAP: usize = 50_000;

/// What `/memory index status` reports.
#[derive(Debug, Clone)]
pub struct MemoryIndexStatus {
    pub fts: usize,
    pub vectors: usize,
    pub model: String,
    pub dim: usize,
    pub stale: bool,
    pub recall_enabled: bool,
}

impl Runtime {
    /// The configured embedder, built lazily and reused until the config
    /// fields that define it change.
    pub fn embedder(&self) -> Result<Option<Arc<dyn Embedder>>, Error> {
        let cfg = self.cfg();
        let key = format!(
            "{}|{}|{}|{}",
            cfg.memory.embeddings.provider,
            cfg.memory.embeddings.base_url.as_deref().unwrap_or(""),
            cfg.memory.embeddings.model.as_deref().unwrap_or(""),
            cfg.memory.embeddings.dimensions.unwrap_or(0)
        );
        {
            let slot = self.sh.embedder.lock().unwrap_or_else(|e| e.into_inner());
            if let Some((cached, embedder)) = slot.as_ref()
                && cached == &key
            {
                return Ok(Some(Arc::clone(embedder)));
            }
        }
        let built = memory_embed::build_embedder(&cfg, &self.sh.paths)
            .map_err(|e| Error::Refused(e.to_string()))?;
        let mut slot = self.sh.embedder.lock().unwrap_or_else(|e| e.into_inner());
        *slot = built.as_ref().map(|e| (key, Arc::clone(e)));
        Ok(built)
    }

    fn vector_meta(&self, username: &str, field: &str) -> Result<Option<String>, Error> {
        Ok(self
            .sh
            .store
            .memory_index_meta(&format!("memory.vectors.{username}.{field}"))?)
    }

    /// `/memory index status`: counts, model, dimension, and whether the
    /// vectors still match the Markdown source.
    pub fn memory_index_status_for(&self, who: &Principal) -> Result<MemoryIndexStatus, Error> {
        let cfg = self.cfg();
        let fts = self.sh.store.count_memory_fts_for_user(&who.username)?;
        let vectors = self.sh.store.count_memory_vectors_for_user(&who.username)?;
        // Status must not load a local model or call an HTTP endpoint.
        let provider = cfg.memory.embeddings.provider.trim().to_ascii_lowercase();
        let enabled = !matches!(provider.as_str(), "" | "none");
        let model = cfg
            .memory
            .embeddings
            .model
            .clone()
            .unwrap_or_else(|| "none".into());
        let configured_dim = cfg.memory.embeddings.dimensions.unwrap_or(0) as usize;
        let dim = self
            .vector_meta(&who.username, "dim")?
            .and_then(|v| v.parse::<usize>().ok())
            .unwrap_or(configured_dim);
        let (fingerprint, entries) = self.memory_entries_with_fingerprint(&who.username)?;
        let expected = entries.len().min(MEMORY_VECTOR_CAP);
        let stale = if enabled {
            // An empty memory has no dimension to compare; model and
            // fingerprint already say whether the (empty) index is current.
            let dim_ok = expected == 0 || configured_dim == 0 || dim == configured_dim;
            vectors != expected
                || !dim_ok
                || self.vector_meta(&who.username, "model")?.as_deref() != Some(model.as_str())
                || self.vector_meta(&who.username, "fingerprint")?.as_deref()
                    != Some(fingerprint.as_str())
        } else {
            false
        };
        Ok(MemoryIndexStatus {
            fts,
            vectors,
            model,
            dim,
            stale,
            recall_enabled: cfg.memory.recall.enabled,
        })
    }

    /// `/memory index rebuild`: rebuild the FTS mirror and, when an
    /// embedder is configured, the vectors.
    pub fn rebuild_memory_index_for(&self, who: &Principal) -> Result<MemoryIndexStatus, Error> {
        self.ensure_memory_index_for(&who.username)?;
        self.rebuild_vectors_for(&who.username)?;
        self.memory_index_status_for(who)
    }

    fn rebuild_vectors_for(&self, username: &str) -> Result<(), Error> {
        let Some(embedder) = self.embedder()? else {
            self.sh.store.delete_memory_vectors_for_user(username)?;
            self.sh.clear_vector_cache(username);
            return Ok(());
        };
        let (fingerprint, mut entries) = self.memory_entries_with_fingerprint(username)?;
        entries.sort_by_key(|e| std::cmp::Reverse(e.accepted_ms));
        entries.truncate(MEMORY_VECTOR_CAP);
        if entries.is_empty() {
            self.sh.store.delete_memory_vectors_for_user(username)?;
            self.sh.store.set_memory_index_meta(
                &format!("memory.vectors.{username}.fingerprint"),
                &fingerprint,
            )?;
            self.sh.store.set_memory_index_meta(
                &format!("memory.vectors.{username}.model"),
                embedder.model(),
            )?;
            self.sh.clear_vector_cache(username);
            return Ok(());
        }
        let batch = self.cfg().memory.embeddings.batch_size.clamp(1, 128) as usize;
        let mut rows = Vec::with_capacity(entries.len());
        let mut dim = 0usize;
        for chunk in entries.chunks(batch) {
            let texts: Vec<String> = chunk.iter().map(|e| e.text.clone()).collect();
            let vectors = embedder
                .embed(&texts)
                .map_err(|e| Error::Refused(e.to_string()))?;
            if vectors.len() != chunk.len() {
                return Err(Error::Refused(format!(
                    "embedder returned {} vectors for {} texts",
                    vectors.len(),
                    chunk.len()
                )));
            }
            for (entry, mut vec) in chunk.iter().zip(vectors) {
                if dim == 0 {
                    dim = vec.len();
                }
                if vec.len() != dim {
                    return Err(Error::Refused(format!(
                        "embedder mixed dimensions: {} and {dim}",
                        vec.len()
                    )));
                }
                if !normalize(&mut vec) {
                    return Err(Error::Refused(format!(
                        "embedder returned a zero vector for {}",
                        entry.id
                    )));
                }
                rows.push(ka_agent_store::MemoryVectorRow {
                    ref_id: entry.id.clone(),
                    scope: Some(entry.file.clone()),
                    vec,
                    updated_ms: entry.accepted_ms,
                });
            }
        }
        if embedder.dim() > 0 && embedder.dim() != dim {
            return Err(Error::Refused(format!(
                "embedder configured for {} dimensions, returned {dim}",
                embedder.dim()
            )));
        }
        self.sh
            .store
            .replace_memory_vectors_for_user(username, embedder.model(), dim, &rows)?;
        for (field, value) in [
            ("model", embedder.model().to_string()),
            ("dim", dim.to_string()),
            ("fingerprint", fingerprint),
            ("updated_ms", ka_agent_core::now_ms().to_string()),
        ] {
            self.sh
                .store
                .set_memory_index_meta(&format!("memory.vectors.{username}.{field}"), &value)?;
        }
        self.sh.clear_vector_cache(username);
        Ok(())
    }

    fn ensure_vector_index_for(&self, username: &str) -> Result<Option<Arc<dyn Embedder>>, Error> {
        let Some(embedder) = self.embedder()? else {
            return Ok(None);
        };
        let (fingerprint, entries) = self.memory_entries_with_fingerprint(username)?;
        let expected = entries.len().min(MEMORY_VECTOR_CAP);
        let count = self.sh.store.count_memory_vectors_for_user(username)?;
        let model_ok = self.vector_meta(username, "model")?.as_deref() == Some(embedder.model());
        let fp_ok =
            self.vector_meta(username, "fingerprint")?.as_deref() == Some(fingerprint.as_str());
        let dim_ok = embedder.dim() == 0
            || self
                .vector_meta(username, "dim")?
                .and_then(|v| v.parse::<usize>().ok())
                == Some(embedder.dim());
        if count == expected && model_ok && fp_ok && dim_ok {
            Ok(Some(embedder))
        } else {
            // Do not block a turn on embedding I/O. The refresh path
            // schedules a background rebuild; until it lands, recall
            // falls back to FTS.
            Ok(None)
        }
    }

    /// Schedule a background vector rebuild after a memory change. A
    /// blocking HTTP embedder runs on the blocking pool, never on the
    /// async runtime's worker threads.
    pub(super) fn spawn_vector_reindex(&self, username: &str) {
        if self.cfg().memory.embeddings.provider == "none" {
            return;
        }
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let rt = self.clone();
        let username = username.to_string();
        handle.spawn_blocking(move || {
            if let Err(e) = rt.rebuild_vectors_for(&username) {
                eprintln!("memory vector reindex: {e}");
            }
        });
    }

    fn cached_vectors(
        &self,
        username: &str,
        model: &str,
        dim: usize,
    ) -> Result<Arc<CachedVectors>, Error> {
        if let Some(cached) = self
            .sh
            .vector_cache
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(username)
            .filter(|c| c.model == model && (dim == 0 || c.dim == dim))
            .cloned()
        {
            return Ok(cached);
        }
        let rows = self
            .sh
            .store
            .memory_vectors_for_user(username, model, MEMORY_VECTOR_CAP)?;
        let dim = rows.first().map(|r| r.vec.len()).unwrap_or(dim);
        let cached = Arc::new(CachedVectors {
            model: model.to_string(),
            dim,
            rows: rows.into_iter().map(|r| (r.ref_id, r.vec)).collect(),
        });
        self.sh
            .vector_cache
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(username.to_string(), Arc::clone(&cached));
        Ok(cached)
    }

    /// Exact cosine top-k over one user's current vectors.
    pub fn vector_top_k_for(
        &self,
        username: &str,
        query: &str,
        k: usize,
    ) -> Result<Vec<(String, f32)>, Error> {
        let Some(embedder) = self.ensure_vector_index_for(username)? else {
            return Ok(Vec::new());
        };
        let mut query_vec = embedder
            .embed_query(query)
            .map_err(|e| Error::Refused(e.to_string()))?;
        if !normalize(&mut query_vec) {
            return Ok(Vec::new());
        }
        let cached = self.cached_vectors(username, embedder.model(), embedder.dim())?;
        Ok(cosine_top_k(&query_vec, &cached.rows, k))
    }
}

fn normalize(vec: &mut [f32]) -> bool {
    let norm: f32 = vec.iter().map(|v| v * v).sum::<f32>().sqrt();
    if !norm.is_finite() || norm <= f32::EPSILON {
        return false;
    }
    for v in vec.iter_mut() {
        *v /= norm;
    }
    true
}

/// Exact cosine top-k. The vectors are expected to be unit length.
pub fn cosine_top_k(query: &[f32], rows: &[(String, Vec<f32>)], k: usize) -> Vec<(String, f32)> {
    let mut scored: Vec<(f32, &str)> = rows
        .iter()
        .filter(|(_, vec)| vec.len() == query.len())
        .map(|(id, vec)| {
            let score: f32 = query.iter().zip(vec).map(|(a, b)| a * b).sum();
            (score, id.as_str())
        })
        .filter(|(score, _)| *score > 0.0)
        .collect();
    scored.sort_by(|a, b| b.0.total_cmp(&a.0));
    scored
        .into_iter()
        .take(k)
        .map(|(score, id)| (id.to_string(), score))
        .collect()
}

/// Reciprocal rank fusion over ranked id lists. `k = 60` is the usual
/// constant; a document that appears in several channels sums their
/// reciprocal ranks.
pub fn rrf_fuse(ranked: &[Vec<String>], limit: usize) -> Vec<(String, f64)> {
    let mut scores: std::collections::HashMap<String, f64> = std::collections::HashMap::new();
    for list in ranked {
        for (rank, id) in list.iter().enumerate() {
            *scores.entry(id.clone()).or_default() += 1.0 / (60.0 + (rank + 1) as f64);
        }
    }
    let mut out: Vec<(String, f64)> = scores.into_iter().collect();
    out.sort_by(|a, b| b.1.total_cmp(&a.1));
    out.truncate(limit);
    out
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::{cosine_top_k, rrf_fuse};

    #[test]
    fn cosine_top_k_orders_by_similarity() {
        let rows = vec![
            ("a".to_string(), vec![1.0, 0.0]),
            ("b".to_string(), vec![0.0, 1.0]),
            ("c".to_string(), vec![0.7, 0.7]),
        ];
        let hits = cosine_top_k(&[1.0, 0.0], &rows, 2);
        assert_eq!(hits[0].0, "a");
        assert_eq!(hits[1].0, "c");
    }

    #[test]
    fn rrf_sums_channels_and_breaks_ties_by_rank() {
        let fused = rrf_fuse(
            &[vec!["a".into(), "b".into()], vec!["b".into(), "c".into()]],
            10,
        );
        assert_eq!(fused[0].0, "b");
        assert!(fused.iter().any(|(id, _)| id == "a"));
        assert!(fused.iter().any(|(id, _)| id == "c"));
    }
}
