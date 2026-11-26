//! Optional embedding backends for memory recall (B1).
//!
//! The default is `none`. An OpenAI-compatible HTTP endpoint adds no big
//! dependency; `local` uses fastembed behind the `local-embeddings`
//! feature, and the Qwen3-Embedding models add `local-qwen3` on top. The
//! runtime normalizes the returned vectors and owns the index; this
//! module only talks to the model.

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
    /// Embed one query for the vector scan. A backend that prompts
    /// queries differently from documents overrides this; the default
    /// embeds the text as given.
    fn embed_query(&self, query: &str) -> Result<Vec<f32>, EmbedError> {
        let mut vectors = self.embed(&[query.to_string()])?;
        vectors
            .pop()
            .ok_or_else(|| EmbedError::Other("embedder returned no query vector".into()))
    }
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
        other => return build_qwen3(other, emb, paths),
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
            .embed(texts, None)
            .map_err(|e| EmbedError::Other(format!("local embed: {e}")))
    }
}

/// One Qwen3-Embedding model (`local-qwen3`).
#[cfg(feature = "local-qwen3")]
struct Qwen3Model {
    /// The `memory.embeddings.model` name.
    name: &'static str,
    /// Hub repo with the Q8_0 GGUF weights: half the f16 download, and
    /// within 0.5% cosine of the f32 model (fastembed's own measurement).
    gguf_repo: &'static str,
    gguf_file: &'static str,
    /// Hub repo with the matching `tokenizer.json`.
    tokenizer_repo: &'static str,
    /// Vector length at full size.
    dim: usize,
}

#[cfg(feature = "local-qwen3")]
const QWEN3_MODELS: &[Qwen3Model] = &[Qwen3Model {
    name: "qwen3-embedding-0.6b",
    gguf_repo: "Qwen/Qwen3-Embedding-0.6B-GGUF",
    gguf_file: "Qwen3-Embedding-0.6B-Q8_0.gguf",
    tokenizer_repo: "Qwen/Qwen3-Embedding-0.6B",
    dim: 1024,
}];

/// The instruction Qwen3-Embedding prefixes to a retrieval query. Its
/// model card recommends one, written for the task at hand; documents go
/// in as they are.
#[cfg(any(feature = "local-qwen3", test))]
const QWEN3_QUERY_INSTRUCTION: &str =
    "Given a question about the user, retrieve the memory notes that answer it";

/// `query` in the Qwen3 query format, `Instruct: <task>\nQuery:<query>`.
#[cfg(any(feature = "local-qwen3", test))]
fn qwen3_query_prompt(query: &str) -> String {
    format!("Instruct: {QWEN3_QUERY_INSTRUCTION}\nQuery:{query}")
}

/// fastembed's Qwen3 loader takes its cache directory from the
/// `FASTEMBED_CACHE_DIR` environment variable, not from the caller, so it
/// would not use `<data>/models`. The Qwen3 path downloads into the model
/// cache itself and loads the weights with the path-taking `from_gguf`.
#[cfg(feature = "local-qwen3")]
fn build_qwen3(
    name: &str,
    emb: &ka_agent_core::config::MemoryEmbeddings,
    paths: &Paths,
) -> Result<Option<Arc<dyn Embedder>>, EmbedError> {
    use candle_core::Device;
    use fastembed::Qwen3TextEmbedding;

    /// Tokens per text. Memory bullets and recall queries are short; this
    /// bounds the attention cost of a long one, as it does for the ONNX
    /// models.
    const MAX_TOKENS: usize = 512;

    let Some(spec) = qwen3_spec(name) else {
        return Err(unknown_local_model(name));
    };
    if emb.dimensions.is_some_and(|d| d as usize != spec.dim) {
        return Err(EmbedError::Other(format!(
            "memory.embeddings.dimensions does not match {name}'s {}",
            spec.dim
        )));
    }
    let cache_dir = model_cache_dir(paths);
    let weights = hub_file(&cache_dir, spec.gguf_repo, spec.gguf_file)?;
    let tokenizer = hub_file(&cache_dir, spec.tokenizer_repo, "tokenizer.json")?;
    let inner = Qwen3TextEmbedding::from_gguf(&weights, &tokenizer, &Device::Cpu, MAX_TOKENS)
        .map_err(|e| EmbedError::Other(format!("qwen3 embedder: {e}")))?;
    Ok(Some(Arc::new(Qwen3Embedder {
        name: name.to_string(),
        dim: spec.dim,
        inner,
    })))
}

#[cfg(all(feature = "local-embeddings", not(feature = "local-qwen3")))]
fn build_qwen3(
    name: &str,
    _emb: &ka_agent_core::config::MemoryEmbeddings,
    _paths: &Paths,
) -> Result<Option<Arc<dyn Embedder>>, EmbedError> {
    Err(EmbedError::Other(format!(
        "unknown local embedding model {name:?}: embeddinggemma-300m | embeddinggemma-300m-q4 | all-minilm-l6-v2 (qwen3-embedding-0.6b needs a build with --features local-qwen3)"
    )))
}

/// `name`'s Qwen3-Embedding row, when it names one.
#[cfg(feature = "local-qwen3")]
fn qwen3_spec(name: &str) -> Option<&'static Qwen3Model> {
    QWEN3_MODELS.iter().find(|model| model.name == name)
}

#[cfg(feature = "local-qwen3")]
fn unknown_local_model(name: &str) -> EmbedError {
    EmbedError::Other(format!(
        "unknown local embedding model {name:?}: embeddinggemma-300m | embeddinggemma-300m-q4 | all-minilm-l6-v2 | qwen3-embedding-0.6b"
    ))
}

/// The model cache: `HF_HOME` wins, as it does for the ONNX models
/// fastembed downloads itself.
#[cfg(feature = "local-qwen3")]
fn model_cache_dir(paths: &Paths) -> std::path::PathBuf {
    std::env::var_os("HF_HOME")
        .filter(|home| !home.is_empty())
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| paths.root().join("models"))
}

/// The path of `file` from `repo` in `cache_dir`, downloaded on first
/// use. `HF_ENDPOINT` and `HF_TOKEN` apply as they do to fastembed's own
/// downloads.
#[cfg(feature = "local-qwen3")]
fn hub_file(
    cache_dir: &std::path::Path,
    repo: &str,
    file: &str,
) -> Result<std::path::PathBuf, EmbedError> {
    use hf_hub::{Cache, api::sync::ApiBuilder};

    let builder = ApiBuilder::from_cache(Cache::new(cache_dir.to_path_buf()))
        .with_progress(false)
        // Set explicitly: unset, the builder would read a `token` file
        // next to the cache directory.
        .with_token(
            std::env::var("HF_TOKEN")
                .ok()
                .map(|token| token.trim().to_string())
                .filter(|token| !token.is_empty()),
        );
    let builder = match std::env::var("HF_ENDPOINT") {
        Ok(endpoint) if !endpoint.trim().is_empty() => builder.with_endpoint(endpoint),
        _ => builder,
    };
    let api = builder
        .build()
        .map_err(|e| EmbedError::Other(format!("huggingface client: {e}")))?;
    api.model(repo.to_string())
        .get(file)
        .map_err(|e| EmbedError::Other(format!("download {repo}/{file}: {e}")))
}

#[cfg(feature = "local-qwen3")]
struct Qwen3Embedder {
    name: String,
    dim: usize,
    /// `embed` takes `&self`, so calls run in parallel without a lock.
    inner: fastembed::Qwen3TextEmbedding,
}

#[cfg(feature = "local-qwen3")]
impl Embedder for Qwen3Embedder {
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
        self.inner
            .embed(texts)
            .map_err(|e| EmbedError::Other(format!("qwen3 embed: {e}")))
    }

    fn embed_query(&self, query: &str) -> Result<Vec<f32>, EmbedError> {
        let mut vectors = self.embed(&[qwen3_query_prompt(query)])?;
        vectors
            .pop()
            .ok_or_else(|| EmbedError::Other("qwen3 returned no query vector".into()))
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::{EmbedError, Embedder, embeddings_url, qwen3_query_prompt};

    /// One zero vector per text, `Some(width)` numbers wide; `None` is
    /// a backend that answers nothing.
    struct StubEmbedder(Option<usize>);

    impl Embedder for StubEmbedder {
        fn model(&self) -> &str {
            "stub"
        }

        fn dim(&self) -> usize {
            self.0.unwrap_or(0)
        }

        fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, EmbedError> {
            let Some(width) = self.0 else {
                return Ok(Vec::new());
            };
            Ok(texts.iter().map(|_| vec![0.0; width]).collect())
        }
    }

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

    #[test]
    fn the_default_query_embedding_is_the_query_embedded_as_given() {
        assert_eq!(
            StubEmbedder(Some(2)).embed_query("ka_staging").unwrap(),
            vec![0.0, 0.0]
        );
        // A backend that answers nothing is an error, not an empty vector.
        assert!(StubEmbedder(None).embed_query("ka_staging").is_err());
    }

    #[test]
    fn qwen3_queries_carry_the_instruction() {
        assert_eq!(
            qwen3_query_prompt("staging server"),
            "Instruct: Given a question about the user, retrieve the memory notes that answer it\nQuery:staging server"
        );
    }

    #[cfg(feature = "local-qwen3")]
    #[test]
    fn qwen3_names_resolve_and_others_do_not() {
        let spec = super::qwen3_spec("qwen3-embedding-0.6b").expect("qwen3 row");
        assert_eq!(spec.dim, 1024);
        assert!(super::qwen3_spec("qwen3-embedding-4b").is_none());
        assert!(super::qwen3_spec("Qwen3-Embedding-0.6B").is_none());
    }
}
