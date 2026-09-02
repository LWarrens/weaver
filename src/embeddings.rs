use text_splitter::TextSplitter;

/// Pack a slice of f32 values into a little-endian byte blob.
pub fn pack_f32(values: &[f32]) -> Vec<u8> {
    let mut buf = Vec::with_capacity(values.len() * 4);
    for &v in values {
        buf.extend_from_slice(&v.to_le_bytes());
    }
    buf
}

/// Unpack a little-endian byte blob into f32 values.
pub fn unpack_f32(blob: &[u8]) -> Vec<f32> {
    blob.chunks_exact(4)
        .map(|b| f32::from_le_bytes(b.try_into().unwrap()))
        .collect()
}

/// Cosine similarity between two equal-length vectors. Returns 0.0 on zero-length inputs.
pub fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() || a.is_empty() {
        return 0.0;
    }
    let dot: f32 = a.iter().zip(b.iter()).map(|(x, y)| x * y).sum();
    let mag_a: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
    let mag_b: f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt();
    if mag_a == 0.0 || mag_b == 0.0 {
        return 0.0;
    }
    dot / (mag_a * mag_b)
}

// ---------------------------------------------------------------------------
// Provider trait
// ---------------------------------------------------------------------------

#[async_trait::async_trait]
pub trait EmbeddingProvider: Send + Sync {
    /// Embed a single piece of text. Implementations call the remote model.
    async fn embed(&self, text: &str) -> anyhow::Result<Vec<f32>>;

    /// Split `text` into chunks of at most `max_chars`, embed each, and return
    /// the component-wise average. Override only when the provider has native
    /// batching; the default is correct for all current backends.
    async fn embed_chunked(&self, text: &str, max_chars: usize) -> anyhow::Result<Vec<f32>> {
        let splitter = TextSplitter::new(max_chars);
        let chunks: Vec<&str> = splitter.chunks(text).collect();
        let chunks = if chunks.is_empty() { vec![text] } else { chunks };

        let mut sum: Vec<f32> = Vec::new();
        let mut count = 0usize;

        for chunk in chunks {
            let chunk = chunk.trim();
            if chunk.is_empty() {
                continue;
            }
            match self.embed(chunk).await {
                Ok(vec) if !vec.is_empty() => {
                    if sum.is_empty() {
                        sum = vec;
                    } else if sum.len() == vec.len() {
                        for (s, v) in sum.iter_mut().zip(vec.iter()) {
                            *s += v;
                        }
                    }
                    count += 1;
                }
                _ => {}
            }
        }

        if count == 0 || sum.is_empty() {
            anyhow::bail!("no chunks produced a valid embedding");
        }
        let n = count as f32;
        Ok(sum.into_iter().map(|v| v / n).collect())
    }
}

// ---------------------------------------------------------------------------
// OllamaEmbeddingProvider
// ---------------------------------------------------------------------------

pub struct OllamaEmbeddingProvider {
    base_url: String,
    model: String,
    client: reqwest::Client,
}

impl OllamaEmbeddingProvider {
    pub fn new(base_url: String, model: String) -> Self {
        Self {
            base_url,
            model,
            client: reqwest::Client::new(),
        }
    }

    pub fn from_env() -> Option<Self> {
        let url = std::env::var("WEAVER_EMBEDDING_URL")
            .unwrap_or_else(|_| "http://127.0.0.1:11434".to_string());
        let model = std::env::var("WEAVER_EMBEDDING_MODEL")
            .unwrap_or_else(|_| "nomic-embed-text".to_string());
        Some(Self::new(url, model))
    }
}

#[async_trait::async_trait]
impl EmbeddingProvider for OllamaEmbeddingProvider {
    async fn embed(&self, text: &str) -> anyhow::Result<Vec<f32>> {
        let url = format!(
            "{}/api/embeddings",
            self.base_url.trim_end_matches('/')
        );
        let body = serde_json::json!({ "model": self.model, "prompt": text });
        let resp: serde_json::Value = self
            .client
            .post(&url)
            .json(&body)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;

        let embedding = resp
            .get("embedding")
            .and_then(|v| v.as_array())
            .ok_or_else(|| anyhow::anyhow!("ollama response missing 'embedding' field"))?
            .iter()
            .map(|v| v.as_f64().unwrap_or(0.0) as f32)
            .collect();

        Ok(embedding)
    }
}

// ---------------------------------------------------------------------------
// OpenAIEmbeddingProvider
// ---------------------------------------------------------------------------

pub struct OpenAIEmbeddingProvider {
    api_key: String,
    model: String,
    client: reqwest::Client,
}

impl OpenAIEmbeddingProvider {
    pub fn new(api_key: String, model: String) -> Self {
        Self {
            api_key,
            model,
            client: reqwest::Client::new(),
        }
    }

    pub fn from_env() -> Option<Self> {
        let api_key = std::env::var("OPENAI_API_KEY").ok()?;
        let model = std::env::var("WEAVER_EMBEDDING_MODEL")
            .unwrap_or_else(|_| "text-embedding-3-small".to_string());
        Some(Self::new(api_key, model))
    }
}

#[async_trait::async_trait]
impl EmbeddingProvider for OpenAIEmbeddingProvider {
    async fn embed(&self, text: &str) -> anyhow::Result<Vec<f32>> {
        let body = serde_json::json!({ "input": text, "model": self.model });
        let resp: serde_json::Value = self
            .client
            .post("https://api.openai.com/v1/embeddings")
            .bearer_auth(&self.api_key)
            .json(&body)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;

        let embedding = resp
            .pointer("/data/0/embedding")
            .and_then(|v| v.as_array())
            .ok_or_else(|| anyhow::anyhow!("openai response missing 'data[0].embedding'"))?
            .iter()
            .map(|v| v.as_f64().unwrap_or(0.0) as f32)
            .collect();

        Ok(embedding)
    }
}

// ---------------------------------------------------------------------------
// LmStudioEmbeddingProvider — OpenAI-compatible, no API key required
// ---------------------------------------------------------------------------

pub struct LmStudioEmbeddingProvider {
    base_url: String,
    model: String,
    client: reqwest::Client,
}

impl LmStudioEmbeddingProvider {
    pub fn new(base_url: String, model: String) -> Self {
        Self {
            base_url,
            model,
            client: reqwest::Client::new(),
        }
    }

    pub fn from_env() -> Option<Self> {
        let url = std::env::var("WEAVER_EMBEDDING_URL")
            .unwrap_or_else(|_| "http://127.0.0.1:1234".to_string());
        // Model is required for LM Studio — no built-in default.
        let model = std::env::var("WEAVER_EMBEDDING_MODEL").ok()?;
        Some(Self::new(url, model))
    }
}

#[async_trait::async_trait]
impl EmbeddingProvider for LmStudioEmbeddingProvider {
    async fn embed(&self, text: &str) -> anyhow::Result<Vec<f32>> {
        let url = format!(
            "{}/v1/embeddings",
            self.base_url.trim_end_matches('/')
        );
        let body = serde_json::json!({ "input": text, "model": self.model });
        let resp: serde_json::Value = self
            .client
            .post(&url)
            .json(&body)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;

        let embedding = resp
            .pointer("/data/0/embedding")
            .and_then(|v| v.as_array())
            .ok_or_else(|| anyhow::anyhow!("lmstudio response missing 'data[0].embedding'"))?
            .iter()
            .map(|v| v.as_f64().unwrap_or(0.0) as f32)
            .collect();

        Ok(embedding)
    }
}

// ---------------------------------------------------------------------------
// Factory
// ---------------------------------------------------------------------------

/// Build an embedding provider from environment variables.
/// Returns `None` when no provider is configured (graceful degradation).
///
/// Provider names (WEAVER_EMBEDDING_PROVIDER):
///   "lmstudio" — LM Studio local server (OpenAI-compatible, no key)
///   "ollama"   — Ollama local server
///   "openai"   — OpenAI API (requires OPENAI_API_KEY)
pub fn provider_from_env() -> Option<Box<dyn EmbeddingProvider>> {
    let provider_name = std::env::var("WEAVER_EMBEDDING_PROVIDER").unwrap_or_default();
    match provider_name.to_ascii_lowercase().as_str() {
        "lmstudio" => LmStudioEmbeddingProvider::from_env()
            .map(|p| Box::new(p) as Box<dyn EmbeddingProvider>),
        "ollama" => OllamaEmbeddingProvider::from_env()
            .map(|p| Box::new(p) as Box<dyn EmbeddingProvider>),
        "openai" => OpenAIEmbeddingProvider::from_env()
            .map(|p| Box::new(p) as Box<dyn EmbeddingProvider>),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pack_unpack_roundtrip() {
        let original = vec![0.1f32, 0.2, 0.3, -0.5];
        let blob = pack_f32(&original);
        let recovered = unpack_f32(&blob);
        assert_eq!(original.len(), recovered.len());
        for (a, b) in original.iter().zip(recovered.iter()) {
            assert!((a - b).abs() < 1e-7);
        }
    }

    #[test]
    fn cosine_similarity_identical() {
        let v = vec![1.0f32, 0.0, 0.0];
        assert!((cosine_similarity(&v, &v) - 1.0).abs() < 1e-6);
    }

    #[test]
    fn cosine_similarity_orthogonal() {
        let a = vec![1.0f32, 0.0];
        let b = vec![0.0f32, 1.0];
        assert!(cosine_similarity(&a, &b).abs() < 1e-6);
    }

    #[test]
    fn provider_from_env_returns_none_when_not_configured() {
        std::env::remove_var("WEAVER_EMBEDDING_PROVIDER");
        assert!(provider_from_env().is_none());
    }

    #[tokio::test]
    async fn embed_chunked_averages_chunks() {
        struct FakeProvider;
        #[async_trait::async_trait]
        impl EmbeddingProvider for FakeProvider {
            async fn embed(&self, text: &str) -> anyhow::Result<Vec<f32>> {
                // Return [1.0, 0.0] for short text, [0.0, 1.0] for long text
                if text.len() < 20 {
                    Ok(vec![1.0, 0.0])
                } else {
                    Ok(vec![0.0, 1.0])
                }
            }
        }

        // Two short chunks → each returns [1.0, 0.0] → average is [1.0, 0.0]
        let result = FakeProvider.embed_chunked("hello world", 6).await.unwrap();
        assert_eq!(result.len(), 2);
        assert!((result[0] - 1.0).abs() < 1e-6);
        assert!(result[1].abs() < 1e-6);
    }
}
