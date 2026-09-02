use async_trait::async_trait;

// ---------------------------------------------------------------------------
// Trait
// ---------------------------------------------------------------------------

#[async_trait]
pub trait LlmProvider: Send + Sync {
    /// Send a prompt and return the model's text response.
    async fn generate(&self, prompt: &str) -> anyhow::Result<String>;
}

// ---------------------------------------------------------------------------
// Ollama
// ---------------------------------------------------------------------------

pub struct OllamaLlmProvider {
    base_url: String,
    model: String,
    client: reqwest::Client,
}

impl OllamaLlmProvider {
    pub fn new(base_url: String, model: String) -> Self {
        Self { base_url, model, client: reqwest::Client::new() }
    }

    pub fn from_env() -> Option<Self> {
        let url = std::env::var("WEAVER_OLLAMA_URL")
            .unwrap_or_else(|_| "http://127.0.0.1:11434".into());
        let model = std::env::var("WEAVER_LLM_MODEL")
            .unwrap_or_else(|_| "llama3.2".to_string());
        Some(Self::new(url, model))
    }
}

#[async_trait]
impl LlmProvider for OllamaLlmProvider {
    async fn generate(&self, prompt: &str) -> anyhow::Result<String> {
        let url = format!("{}/api/generate", self.base_url.trim_end_matches('/'));
        let body = serde_json::json!({
            "model": self.model,
            "prompt": prompt,
            "stream": false,
            "format": "json"
        });
        let value: serde_json::Value = self
            .client
            .post(&url)
            .json(&body)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        value
            .get("response")
            .and_then(|v| v.as_str())
            .map(str::to_string)
            .ok_or_else(|| anyhow::anyhow!("ollama response missing 'response' field"))
    }
}

// ---------------------------------------------------------------------------
// OpenAI
// ---------------------------------------------------------------------------

pub struct OpenAILlmProvider {
    api_key: String,
    model: String,
    client: reqwest::Client,
}

impl OpenAILlmProvider {
    pub fn new(api_key: String, model: String) -> Self {
        Self { api_key, model, client: reqwest::Client::new() }
    }

    pub fn from_env() -> Option<Self> {
        let api_key = std::env::var("OPENAI_API_KEY").ok()?;
        let model = std::env::var("WEAVER_LLM_MODEL")
            .unwrap_or_else(|_| "gpt-4o-mini".to_string());
        Some(Self::new(api_key, model))
    }
}

#[async_trait]
impl LlmProvider for OpenAILlmProvider {
    async fn generate(&self, prompt: &str) -> anyhow::Result<String> {
        let body = serde_json::json!({
            "model": self.model,
            "messages": [{"role": "user", "content": prompt}]
        });
        let value: serde_json::Value = self
            .client
            .post("https://api.openai.com/v1/chat/completions")
            .bearer_auth(&self.api_key)
            .json(&body)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        value
            .pointer("/choices/0/message/content")
            .and_then(|v| v.as_str())
            .map(str::to_string)
            .ok_or_else(|| anyhow::anyhow!("openai response missing 'choices[0].message.content'"))
    }
}

// ---------------------------------------------------------------------------
// LM Studio — OpenAI-compatible chat completions, configurable base URL
// ---------------------------------------------------------------------------

pub struct LmStudioLlmProvider {
    base_url: String,
    model: String,
    api_key: String,
    client: reqwest::Client,
}

impl LmStudioLlmProvider {
    pub fn new(base_url: String, model: String, api_key: String) -> Self {
        Self { base_url, model, api_key, client: reqwest::Client::new() }
    }

    pub fn from_env() -> Option<Self> {
        let url = std::env::var("WEAVER_LLM_URL")
            .unwrap_or_else(|_| "http://127.0.0.1:1234".into());
        let model = std::env::var("WEAVER_LLM_MODEL").ok()?;
        let api_key = std::env::var("WEAVER_LLM_API_KEY")
            .unwrap_or_else(|_| "lm-studio".into());
        Some(Self::new(url, model, api_key))
    }
}

#[async_trait]
impl LlmProvider for LmStudioLlmProvider {
    async fn generate(&self, prompt: &str) -> anyhow::Result<String> {
        let url = format!(
            "{}/v1/chat/completions",
            self.base_url.trim_end_matches('/')
        );
        let body = serde_json::json!({
            "model": self.model,
            "messages": [{"role": "user", "content": prompt}]
        });
        let value: serde_json::Value = self
            .client
            .post(&url)
            .bearer_auth(&self.api_key)
            .json(&body)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        value
            .pointer("/choices/0/message/content")
            .and_then(|v| v.as_str())
            .map(str::to_string)
            .ok_or_else(|| anyhow::anyhow!("lmstudio response missing 'choices[0].message.content'"))
    }
}

// ---------------------------------------------------------------------------
// Mock (tests + local development)
// ---------------------------------------------------------------------------

pub struct MockLlmProvider {
    response: String,
}

impl MockLlmProvider {
    pub fn new(response: String) -> Self {
        Self { response }
    }

    pub fn from_env() -> Option<Self> {
        std::env::var("WEAVER_LLM_RESPONSE").ok().map(Self::new)
    }
}

#[async_trait]
impl LlmProvider for MockLlmProvider {
    async fn generate(&self, _prompt: &str) -> anyhow::Result<String> {
        Ok(self.response.clone())
    }
}

// ---------------------------------------------------------------------------
// Factory
// ---------------------------------------------------------------------------

/// Build an LLM provider from environment variables. Returns `None` when no
/// provider is configured (graceful degradation — callers skip LLM features).
///
/// Provider names (WEAVER_LLM_PROVIDER):
///   "lmstudio" — LM Studio local server (WEAVER_LLM_URL + WEAVER_LLM_MODEL + WEAVER_LLM_API_KEY)
///   "ollama"   — Ollama local server (WEAVER_OLLAMA_URL + WEAVER_LLM_MODEL)
///   "openai"   — OpenAI API (OPENAI_API_KEY + WEAVER_LLM_MODEL)
///   "mock"     — Test/development stub (WEAVER_LLM_RESPONSE)
pub fn provider_from_env() -> Option<Box<dyn LlmProvider>> {
    let name = std::env::var("WEAVER_LLM_PROVIDER").unwrap_or_default();
    match name.to_ascii_lowercase().as_str() {
        "lmstudio" => LmStudioLlmProvider::from_env().map(|p| Box::new(p) as Box<dyn LlmProvider>),
        "ollama" => OllamaLlmProvider::from_env().map(|p| Box::new(p) as Box<dyn LlmProvider>),
        "openai" => OpenAILlmProvider::from_env().map(|p| Box::new(p) as Box<dyn LlmProvider>),
        "mock" => MockLlmProvider::from_env().map(|p| Box::new(p) as Box<dyn LlmProvider>),
        _ => None,
    }
}
