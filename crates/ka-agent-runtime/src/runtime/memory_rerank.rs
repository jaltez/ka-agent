//! Optional reranker for memory recall.
//!
//! `memory.rerank` is off by default. Enabled, a local cross-encoder
//! (fastembed, behind the `local-embeddings` feature) re-scores the
//! candidates that fused FTS + vector ranking produced, so their final
//! order follows how well each answers the query. Scores are the model's
//! raw logits: only their order is meaningful.

use std::sync::Arc;
#[cfg(feature = "local-embeddings")]
use std::sync::Mutex;

use ka_agent_core::DaemonConfig;

use super::{Error, Runtime};
use crate::paths::Paths;

/// One reranking backend.
pub trait Reranker: Send + Sync {
    fn model(&self) -> &str;
    /// Every candidate's relevance to `query`, best first. A candidate is
    /// `(id, text)`; the result keeps every id.
    fn rerank(
        &self,
        query: &str,
        candidates: &[(String, String)],
    ) -> Result<Vec<(String, f32)>, Error>;
}

/// Build the configured reranker. `Ok(None)` is the disabled case
/// (`memory.rerank.enabled = false`): no model loads.
pub fn build_reranker(
    cfg: &DaemonConfig,
    paths: &Paths,
) -> Result<Option<Arc<dyn Reranker>>, Error> {
    if !cfg.memory.rerank.enabled {
        return Ok(None);
    }
    build_local(cfg.memory.rerank.model_name(), paths).map(Some)
}

impl Runtime {
    /// The configured reranker, built lazily and reused until the config
    /// fields that define it change. `Ok(None)` when reranking is off.
    pub fn reranker(&self) -> Result<Option<Arc<dyn Reranker>>, Error> {
        let cfg = self.cfg();
        let key = format!(
            "{}|{}",
            cfg.memory.rerank.enabled,
            cfg.memory.rerank.model_name()
        );
        {
            let slot = self.sh.reranker.lock().unwrap_or_else(|e| e.into_inner());
            if let Some((cached, reranker)) = slot.as_ref()
                && cached == &key
            {
                return Ok(Some(Arc::clone(reranker)));
            }
        }
        let built = build_reranker(&cfg, &self.sh.paths)?;
        let mut slot = self.sh.reranker.lock().unwrap_or_else(|e| e.into_inner());
        *slot = built.as_ref().map(|reranker| (key, Arc::clone(reranker)));
        Ok(built)
    }
}

#[cfg(feature = "local-embeddings")]
fn build_local(name: &str, paths: &Paths) -> Result<Arc<dyn Reranker>, Error> {
    use fastembed::{RerankInitOptions, TextRerank};

    let (model, canonical) = resolve_model(name)?;
    let options = RerankInitOptions::new(model)
        .with_cache_dir(paths.root().join("models"))
        .with_show_download_progress(false);
    let inner =
        TextRerank::try_new(options).map_err(|e| Error::Refused(format!("local reranker: {e}")))?;
    Ok(Arc::new(LocalReranker {
        model: canonical.to_string(),
        inner: Mutex::new(inner),
    }))
}

#[cfg(not(feature = "local-embeddings"))]
fn build_local(_name: &str, _paths: &Paths) -> Result<Arc<dyn Reranker>, Error> {
    Err(Error::Refused(
        "memory.rerank needs a build with --features local-embeddings".into(),
    ))
}

/// `name` to the fastembed reranker and its canonical spelling. The hub
/// organization is optional and the name is case-insensitive.
#[cfg(feature = "local-embeddings")]
fn resolve_model(name: &str) -> Result<(fastembed::RerankerModel, &'static str), Error> {
    use fastembed::RerankerModel::{
        BGERerankerBase, BGERerankerV2M3, JINARerankerV1TurboEn, JINARerankerV2BaseMultilingual,
    };

    let resolved = match name.to_ascii_lowercase().as_str() {
        "bge-reranker-v2-m3" | "baai/bge-reranker-v2-m3" => {
            (BGERerankerV2M3, "BAAI/bge-reranker-v2-m3")
        }
        "bge-reranker-base" | "baai/bge-reranker-base" => {
            (BGERerankerBase, "BAAI/bge-reranker-base")
        }
        "jina-reranker-v1-turbo-en" | "jinaai/jina-reranker-v1-turbo-en" => {
            (JINARerankerV1TurboEn, "jinaai/jina-reranker-v1-turbo-en")
        }
        "jina-reranker-v2-base-multilingual" | "jinaai/jina-reranker-v2-base-multilingual" => (
            JINARerankerV2BaseMultilingual,
            "jinaai/jina-reranker-v2-base-multilingual",
        ),
        other => {
            return Err(Error::Refused(format!(
                "unknown memory.rerank.model {other:?}: bge-reranker-v2-m3 | bge-reranker-base | jina-reranker-v1-turbo-en | jina-reranker-v2-base-multilingual"
            )));
        }
    };
    Ok(resolved)
}

/// Pair each candidate with the score the model gave it and sort, best
/// first. The stable sort keeps the candidate order among equal scores,
/// so rank fusion stays the tie-break.
#[cfg(any(feature = "local-embeddings", test))]
fn order_by_score(
    candidates: &[(String, String)],
    scores: &[(usize, f32)],
) -> Result<Vec<(String, f32)>, Error> {
    if scores.len() != candidates.len() {
        return Err(Error::Refused(format!(
            "reranker returned {} scores for {} candidates",
            scores.len(),
            candidates.len()
        )));
    }
    let mut out: Vec<(String, f32)> = Vec::with_capacity(scores.len());
    for &(index, score) in scores {
        let Some((id, _)) = candidates.get(index) else {
            return Err(Error::Refused(format!(
                "reranker scored candidate {index} of {}",
                candidates.len()
            )));
        };
        if !score.is_finite() {
            return Err(Error::Refused(format!(
                "reranker returned a non-finite score for {id}"
            )));
        }
        out.push((id.clone(), score));
    }
    out.sort_by(|a, b| b.1.total_cmp(&a.1));
    Ok(out)
}

#[cfg(feature = "local-embeddings")]
struct LocalReranker {
    model: String,
    inner: Mutex<fastembed::TextRerank>,
}

#[cfg(feature = "local-embeddings")]
impl Reranker for LocalReranker {
    fn model(&self) -> &str {
        &self.model
    }

    fn rerank(
        &self,
        query: &str,
        candidates: &[(String, String)],
    ) -> Result<Vec<(String, f32)>, Error> {
        if candidates.is_empty() {
            return Ok(Vec::new());
        }
        let texts: Vec<&str> = candidates.iter().map(|(_, text)| text.as_str()).collect();
        let mut inner = self
            .inner
            .lock()
            .map_err(|_| Error::Refused("local reranker lock poisoned".into()))?;
        let scored = inner
            .rerank(query, texts, false, None)
            .map_err(|e| Error::Refused(format!("local rerank: {e}")))?;
        let scores: Vec<(usize, f32)> = scored.iter().map(|r| (r.index, r.score)).collect();
        order_by_score(candidates, &scores)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::{build_reranker, order_by_score};
    use crate::paths::Paths;
    use ka_agent_core::config::{DaemonConfig, Memory, MemoryRerank};

    fn candidates(ids: &[&str]) -> Vec<(String, String)> {
        ids.iter()
            .map(|id| (id.to_string(), format!("text of {id}")))
            .collect()
    }

    fn enabled(model: Option<&str>) -> DaemonConfig {
        DaemonConfig {
            memory: Memory {
                rerank: MemoryRerank {
                    enabled: true,
                    model: model.map(str::to_string),
                },
                ..Memory::default()
            },
            ..DaemonConfig::default()
        }
    }

    #[test]
    fn a_disabled_reranker_is_none_and_touches_no_model() {
        let root = tempfile::tempdir().unwrap();
        let paths = Paths::new(root.path());
        let cfg = DaemonConfig::default();
        assert!(!cfg.memory.rerank.enabled);
        assert!(build_reranker(&cfg, &paths).unwrap().is_none());
        assert!(!root.path().join("models").exists());
    }

    #[test]
    fn candidates_come_back_best_first_and_ties_keep_their_order() {
        let candidates = candidates(&["a", "b", "c"]);
        let ranked = order_by_score(&candidates, &[(0, -1.5), (1, 2.0), (2, 2.0)]).unwrap();
        assert_eq!(
            ranked,
            vec![
                ("b".to_string(), 2.0),
                ("c".to_string(), 2.0),
                ("a".to_string(), -1.5)
            ]
        );
        assert!(order_by_score(&[], &[]).unwrap().is_empty());

        // A count mismatch, an unknown candidate and a broken score are
        // errors, never a silently reordered list.
        assert!(order_by_score(&candidates[..2], &[(0, 0.0)]).is_err());
        assert!(order_by_score(&candidates, &[(0, 0.0), (1, 0.0), (3, 0.0)]).is_err());
        assert!(order_by_score(&candidates, &[(0, f32::NAN), (1, 0.0), (2, 0.0)]).is_err());
    }

    #[cfg(feature = "local-embeddings")]
    #[test]
    fn an_unknown_reranker_is_refused_before_any_download() {
        let root = tempfile::tempdir().unwrap();
        let paths = Paths::new(root.path());
        let err = build_reranker(&enabled(Some("nope")), &paths)
            .err()
            .unwrap()
            .to_string();
        assert!(err.contains("unknown memory.rerank.model"), "{err}");
        assert!(!root.path().join("models").exists());
    }

    #[cfg(feature = "local-embeddings")]
    #[test]
    fn reranker_names_resolve_case_and_organization_insensitively() {
        use super::resolve_model;
        use fastembed::RerankerModel;

        let (model, canonical) = resolve_model("baai/BGE-Reranker-V2-M3").unwrap();
        assert_eq!(model, RerankerModel::BGERerankerV2M3);
        assert_eq!(canonical, "BAAI/bge-reranker-v2-m3");
        assert_eq!(
            resolve_model("BAAI/bge-reranker-base").unwrap().0,
            RerankerModel::BGERerankerBase
        );
        assert_eq!(
            resolve_model("jinaai/jina-reranker-v1-turbo-en").unwrap().0,
            RerankerModel::JINARerankerV1TurboEn
        );
        assert_eq!(
            resolve_model("jina-reranker-v2-base-multilingual")
                .unwrap()
                .0,
            RerankerModel::JINARerankerV2BaseMultilingual
        );
        assert!(resolve_model("rozgo/bge-reranker-v2-m3").is_err());
    }

    #[cfg(not(feature = "local-embeddings"))]
    #[test]
    fn an_enabled_reranker_needs_the_feature_build() {
        let root = tempfile::tempdir().unwrap();
        let paths = Paths::new(root.path());
        let err = build_reranker(&enabled(None), &paths)
            .err()
            .unwrap()
            .to_string();
        assert!(err.contains("--features local-embeddings"), "{err}");
    }
}
