//! Optional embedding backends for memory recall (B1).
//!
//! The default is `none`. An OpenAI-compatible HTTP endpoint adds no big
//! dependency; `local` uses fastembed behind the `local-embeddings`
//! feature. The runtime normalizes the returned vectors and owns the
//! index; this module only talks to the model.

use std::sync::Arc;
#[cfg(feature = "local-embeddings")]
use std::sync::Mutex;

use ka_agent_core::DaemonConfig;
use serde_json::json;

use crate::paths::Paths;

/// One user's vectors as the exact scan sees them.
#[derive(Clone)]
pub struct CachedVectors {
    pub model: String,
    pub dim: usize,
    pub rows: Vec<(String, Vec<f32>)>,
}

#[derive(Debug, thiserror::Error)]
pub enum EmbedError {
    #[error("http: {0}")]
    Http(#[from] reqwest::Error),
    #[error("{0}")]
    Other(String),
}

/// One embedding backend.
pub trait Embedder: Send + Sync {
    fn model(&self) -> &str;
    /// Configured dimension, or 0 when the backend discovers it per call.
    fn dim(&self) -> usize;
    fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, EmbedError>;
}

/// Build the configured backend. `None` is the `provider = "none"` case.
pub fn build_embedder(
    cfg: &DaemonConfig,
    paths: &Paths,
) -> Result<Option<Arc<dyn Embedder>>, EmbedError> {
    let emb = &cfg.memory.embeddings;
    let provider = emb.provider.trim().to_ascii_lowercase();
    match provider.as_str() {
        "" | "none" => Ok(None),
        "openai" | "ollama" => {
            let base = emb
                .base_url
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .ok_or_else(|| {
                    EmbedError::Other("memory.embeddings.base_url is required".into())
                })?;
            let model = emb
                .model
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .ok_or_else(|| EmbedError::Other("memory.embeddings.model is required".into()))?;
            let api_key = resolve_api_key(emb.api_key.as_deref(), paths)?;
            let client = reqwest::blocking::Client::builder()
                .timeout(std::time::Duration::from_secs(
                    emb.timeout_secs.clamp(1, 300),
                ))
                .build()?;
            Ok(Some(Arc::new(HttpEmbedder {
                client,
                url: embeddings_url(base),
                model: model.to_string(),
                configured_dim: emb.dimensions.map(|d| d as usize).unwrap_or(0),
                api_key,
            })))
        }
        "local" => build_local(emb, paths),
        other => Err(EmbedError::Other(format!(
            "unknown memory.embeddings.provider {other:?}: none | openai | ollama | local"
        ))),
    }
}

fn resolve_api_key(raw: Option<&str>, paths: &Paths) -> Result<Option<String>, EmbedError> {
    let Some(raw) = raw.map(str::trim).filter(|s| !s.is_empty()) else {
        return Ok(None);
    };
    if let Some(name) = raw.strip_prefix("secret:") {
        let value = ka_agent_core::secret::resolve(name, &paths.root().join("secrets"))
            .map_err(|e| EmbedError::Other(format!("secret {name}: {e}")))?;
        Ok(Some(value))
    } else {
        Ok(Some(raw.to_string()))
    }
}

/// Append the embeddings path unless the operator already named it.
fn embeddings_url(base: &str) -> String {
    let base = base.trim_end_matches('/');
    if base.ends_with("/embeddings") {
        base.to_string()
    } else if base.ends_with("/v1") {
        format!("{base}/embeddings")
    } else {
        format!("{base}/v1/embeddings")
    }
}

struct HttpEmbedder {
    client: reqwest::blocking::Client,
    url: String,
    model: String,
    configured_dim: usize,
    api_key: Option<String>,
}

impl Embedder for HttpEmbedder {
    fn model(&self) -> &str {
        &self.model
    }

    fn dim(&self) -> usize {
        self.configured_dim
    }

    fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, EmbedError> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }
        let mut req = self
            .client
            .post(&self.url)
            .json(&json!({"model": self.model, "input": texts}));
        if let Some(key) = &self.api_key {
            req = req.bearer_auth(key);
        }
        let response = req.send()?;
        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().unwrap_or_default();
            let body: String = body.chars().take(300).collect();
            return Err(EmbedError::Other(format!(
                "embeddings endpoint returned {status}: {body}"
            )));
        }
        let body: serde_json::Value = response.json()?;
        let data = body["data"]
            .as_array()
            .ok_or_else(|| EmbedError::Other("embeddings response has no data array".into()))?;
        if data.len() != texts.len() {
            return Err(EmbedError::Other(format!(
                "embeddings response has {} vectors for {} texts",
                data.len(),
                texts.len()
            )));
        }
        let mut out: Vec<(usize, Vec<f32>)> = Vec::with_capacity(data.len());
        for (fallback, item) in data.iter().enumerate() {
            let index = item["index"].as_u64().unwrap_or(fallback as u64) as usize;
            let raw = item["embedding"]
                .as_array()
                .ok_or_else(|| EmbedError::Other(format!("vector {index} missing embedding")))?;
            let mut vec = Vec::with_capacity(raw.len());
            for value in raw {
                let value = value
                    .as_f64()
                    .ok_or_else(|| EmbedError::Other(format!("vector {index} has a non-number")))?;
                vec.push(value as f32);
            }
            if vec.is_empty() {
                return Err(EmbedError::Other(format!("vector {index} is empty")));
            }
            if self.configured_dim > 0 && vec.len() != self.configured_dim {
                return Err(EmbedError::Other(format!(
                    "vector {index} has {} dimensions, configured {}",
                    vec.len(),
                    self.configured_dim
                )));
            }
            out.push((index, vec));
        }
        out.sort_by_key(|(index, _)| *index);
        if out
            .iter()
            .enumerate()
            .any(|(expected, (index, _))| expected != *index)
        {
            return Err(EmbedError::Other(
                "embeddings response has missing or duplicate indices".into(),
            ));
        }
        Ok(out.into_iter().map(|(_, vec)| vec).collect())
    }
}

#[cfg(feature = "local-embeddings")]
fn build_local(
    emb: &ka_agent_core::config::MemoryEmbeddings,
    paths: &Paths,
) -> Result<Option<Arc<dyn Embedder>>, EmbedError> {
    use fastembed::{EmbeddingModel, TextEmbedding, TextInitOptions};
    let name = emb
        .model
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| EmbedError::Other("memory.embeddings.model is required".into()))?;
    let (model, dim) = match name {
        "embeddinggemma-300m" => (EmbeddingModel::EmbeddingGemma300M, 768),
        "embeddinggemma-300m-q4" => (EmbeddingModel::EmbeddingGemma300MQ4, 768),
        "all-minilm-l6-v2" => (EmbeddingModel::AllMiniLML6V2, 384),
        other => {
            return Err(EmbedError::Other(format!(
                "unknown local embedding model {other:?}: embeddinggemma-300m | embeddinggemma-300m-q4 | all-minilm-l6-v2"
            )));
        }
    };
    if emb.dimensions.is_some_and(|d| d as usize != dim) {
        return Err(EmbedError::Other(format!(
            "memory.embeddings.dimensions does not match {name}'s {dim}"
        )));
    }
    let mut options = TextInitOptions::new(model).with_show_download_progress(false);
    options = options.with_cache_dir(paths.root().join("models"));
    let inner = TextEmbedding::try_new(options)
        .map_err(|e| EmbedError::Other(format!("local embedder: {e}")))?;
    Ok(Some(Arc::new(LocalEmbedder {
        name: name.to_string(),
        dim,
        inner: Mutex::new(inner),
    })))
}

#[cfg(not(feature = "local-embeddings"))]
fn build_local(
    _emb: &ka_agent_core::config::MemoryEmbeddings,
    _paths: &Paths,
) -> Result<Option<Arc<dyn Embedder>>, EmbedError> {
    Err(EmbedError::Other(
        "provider = local needs a build with --features local-embeddings".into(),
    ))
}

#[cfg(feature = "local-embeddings")]
struct LocalEmbedder {
    name: String,
    dim: usize,
    inner: Mutex<fastembed::TextEmbedding>,
}

#[cfg(feature = "local-embeddings")]
impl Embedder for LocalEmbedder {
    fn model(&self) -> &str {
        &self.name
    }

    fn dim(&self) -> usize {
        self.dim
    }

    fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, EmbedError> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }
        let mut inner = self
            .inner
            .lock()
            .map_err(|_| EmbedError::Other("local embedder lock poisoned".into()))?;
        inner
            .embed(texts.to_vec(), None)
            .map_err(|e| EmbedError::Other(format!("local embed: {e}")))
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::embeddings_url;

    #[test]
    fn url_appends_the_path_once() {
        assert_eq!(
            embeddings_url("http://x:11434"),
            "http://x:11434/v1/embeddings"
        );
        assert_eq!(
            embeddings_url("http://x:11434/v1/"),
            "http://x:11434/v1/embeddings"
        );
        assert_eq!(
            embeddings_url("http://x/openai/v1/embeddings"),
            "http://x/openai/v1/embeddings"
        );
    }
}
