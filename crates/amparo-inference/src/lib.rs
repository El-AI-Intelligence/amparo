//! Amparo Inference — BYO-LLM inference abstraction layer.
//!
//! Originally part of Axiom-OS (MIT, Copyright (c) Pixel Phantom AI);
//! ported to Amparo and relicensed Apache-2.0. See NOTICE at the repo root.

#![warn(missing_docs)]

mod anthropic;

pub mod catalog;

pub use anthropic::AnthropicProvider;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::pin::Pin;
use std::sync::Arc;
use thiserror::Error;

/// A type-erased byte stream returned by `complete_chat_stream`.
/// Each item is a `Result<bytes::Bytes, InferenceError>`.
pub type InferenceStream = Pin<
    Box<dyn futures_util::Stream<Item = std::result::Result<bytes::Bytes, InferenceError>> + Send>,
>;

/// Wrap an item stream with an idle timeout: if no item arrives within `dur`,
/// one final `Err(InferenceError::Timeout)` is emitted and the stream ends.
pub fn stream_with_idle_timeout<S>(stream: S, dur: std::time::Duration) -> InferenceStream
where
    S: futures_util::Stream<Item = std::result::Result<bytes::Bytes, InferenceError>>
        + Send
        + 'static,
{
    use futures_util::StreamExt;
    let stream = Box::pin(stream);
    Box::pin(futures_util::stream::unfold(
        (stream, false),
        move |(mut s, timed_out)| async move {
            if timed_out {
                return None;
            }
            match tokio::time::timeout(dur, s.as_mut().next()).await {
                Ok(Some(item)) => Some((item, (s, false))),
                Ok(None) => None,
                Err(_) => Some((Err(InferenceError::Timeout), (s, true))),
            }
        },
    ))
}

/// Error type for Amparo's BYO-LLM inference layer, surfacing provider
/// failures, invalid requests, configuration errors, and timeouts under one type.
#[derive(Error, Debug)]
pub enum InferenceError {
    /// The upstream provider returned an error or failed the request.
    #[error("Provider error: {0}")]
    Provider(String),
    /// The request is invalid for the configured provider.
    #[error("Invalid request: {0}")]
    InvalidRequest(String),
    /// Configuration is missing or invalid (e.g. a required environment variable).
    #[error("Configuration error: {0}")]
    Config(String),
    /// The request timed out; stream idle timeouts emit this as the final item.
    #[error("Timeout")]
    Timeout,
}

/// Result alias for fallible inference operations, carrying [`InferenceError`].
pub type Result<T> = std::result::Result<T, InferenceError>;

/// Inference request
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct InferenceRequest {
    /// The user prompt to send to the model.
    pub prompt: String,
    /// Maximum tokens to generate; filled with `DEFAULT_MAX_TOKENS` and clamped
    /// to the provider limit when set.
    pub max_tokens: Option<usize>,
    /// Sampling temperature; defaults to 0.7 when unset.
    pub temperature: Option<f32>,
    /// Override the provider's default model for this single request.
    pub model: Option<String>,
    /// Privacy level — determines whether the request may leave the device.
    #[serde(default)]
    pub privacy_level: Option<String>,
    /// Structured output JSON schema (Ollama 0.18+ structured outputs).
    #[serde(default)]
    pub json_schema: Option<serde_json::Value>,
    /// Thinking mode control: "none", "brief", "medium", "full".
    #[serde(default)]
    pub thinking: Option<String>,
    /// Challenge gradient tier (1-3) — controls response creativity/safety.
    /// Level 1: Conservative (low temp), Level 2: Balanced, Level 3: Creative (high temp)
    #[serde(default)]
    pub challenge_level: Option<u8>,
}

// ─────────────────────────────────────────────────── InferenceConfig ─────────

/// The kind of inference provider to use.
// `Copy` is safe (fieldless enum) and lets the catalog's `ProviderSpec`
// derive `Copy` so lookups hand back values without clones.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderKind {
    /// OpenAI-compatible API (vLLM, OpenRouter, Moonshot, …)
    OpenAI,
    /// Anthropic Messages API (`https://api.anthropic.com`).
    Anthropic,
    /// Ollama's native `/api/chat` — used for any host when the provider is
    /// cataloged as `ollama`, not just the localhost sniff.
    OllamaNative,
}

impl Default for ProviderKind {
    fn default() -> Self {
        ProviderKind::OpenAI
    }
}

/// Cloud fallback configuration for when local inference is unavailable.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CloudConfig {
    /// Cloud API endpoint (e.g. `https://api.anthropic.com/v1`)
    pub url: String,
    /// Cloud API key
    pub api_key: String,
    /// Cloud model name (e.g. claude-sonnet-4-20250514)
    pub model: String,
    /// Whether to use cloud as fallback when local is unavailable
    pub enabled: bool,
}

impl Default for CloudConfig {
    fn default() -> Self {
        Self {
            url: "https://api.anthropic.com/v1".to_string(),
            api_key: String::new(),
            model: "claude-sonnet-4-20250514".to_string(),
            enabled: false,
        }
    }
}

impl CloudConfig {
    /// Build a cloud config from the `AMPARO_CLOUD_*` environment variables;
    /// enabled when a key is present or `AMPARO_USE_CLOUD_FALLBACK` is true.
    pub fn from_env() -> Self {
        let api_key = std::env::var("AMPARO_CLOUD_API_KEY").unwrap_or_default();
        Self {
            url: std::env::var("AMPARO_CLOUD_URL")
                .unwrap_or_else(|_| "https://api.anthropic.com/v1".to_string()),
            api_key: api_key.clone(),
            model: std::env::var("AMPARO_CLOUD_MODEL")
                .unwrap_or_else(|_| "claude-sonnet-4-20250514".to_string()),
            enabled: !api_key.is_empty()
                || std::env::var("AMPARO_USE_CLOUD_FALLBACK")
                    .map(|v| v == "1" || v.to_lowercase() == "true")
                    .unwrap_or(false),
        }
    }

    /// Returns true if cloud fallback is configured and available.
    pub fn is_available(&self) -> bool {
        self.enabled && !self.api_key.is_empty()
    }

    /// Build a cloud provider from this config.
    pub fn build_provider(&self) -> Option<Arc<dyn InferenceProvider>> {
        if self.is_available() {
            // Normalize, but keep a badly-shaped URL working as before
            // rather than silently disabling the fallback.
            let url = catalog::normalize_base_url(&self.url)
                .map(|u| catalog::append_v1_if_bare(&u))
                .unwrap_or_else(|_| self.url.clone());
            Some(Arc::new(OpenAIProvider::new(
                url,
                self.api_key.clone(),
                self.model.clone(),
                DEFAULT_TIMEOUT_SECS,
                None,
                ProviderKind::OpenAI,
                false,
            )))
        } else {
            None
        }
    }
}

/// Default HTTP timeout for inference requests, in seconds.
pub const DEFAULT_TIMEOUT_SECS: u64 = 120;
/// Upper bound for `AMPARO_INFERENCE_TIMEOUT_SECS`.
pub const MAX_TIMEOUT_SECS: u64 = 3600;
/// Default `max_tokens` when a request does not specify one.
pub const DEFAULT_MAX_TOKENS: usize = 1024;

/// Clamp a request's `max_tokens` to the configured limit (when one is set).
/// Requests without `max_tokens` get `DEFAULT_MAX_TOKENS`.
pub fn clamp_max_tokens(requested: Option<usize>, limit: Option<usize>) -> usize {
    let requested = requested.unwrap_or(DEFAULT_MAX_TOKENS);
    match limit {
        Some(l) => requested.min(l),
        None => requested,
    }
}

/// Snapshot of provider configuration — can be loaded from env, persisted, and
/// applied to a live `InferenceHandle` via `reconfigure()`.
///
/// There is deliberately **no `Default` implementation** — Amparo never
/// silently dials a hardcoded localhost endpoint. A config must come from
/// `from_env()` (fail-closed) or be constructed explicitly by the embedder.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InferenceConfig {
    /// Provider base URL, e.g. `http://localhost:11434/v1`.
    pub base_url: String,
    /// API key for the provider (empty for keyless local providers).
    pub api_key:  String,
    /// Default model ID used when a request does not specify one.
    pub model:    String,
    /// Provider kind, selecting the wire protocol (OpenAI-compatible or Anthropic).
    #[serde(default)]
    pub provider: ProviderKind,
    /// Catalog id (or alias) the config was resolved from, e.g. `moonshot`.
    /// Drives per-provider behavior flags (reasoning echo); `None` falls
    /// back to the flags of the kind's catalog entry.
    #[serde(default)]
    pub provider_id: Option<String>,
    /// HTTP timeout for a single inference request, in seconds.
    #[serde(default = "default_timeout_secs")]
    pub timeout_secs: u64,
    /// Upper bound on `max_tokens` for every request through this config.
    #[serde(default)]
    pub max_tokens_limit: Option<usize>,
    /// When non-empty, `build()` refuses any model not on this list.
    #[serde(default)]
    pub allowlist: Option<Vec<String>>,
    /// Cloud fallback configuration.
    #[serde(default)]
    pub cloud: CloudConfig,
}

fn default_timeout_secs() -> u64 {
    DEFAULT_TIMEOUT_SECS
}

impl InferenceConfig {
    /// Parse the canonical env surface:
    ///
    /// - `AMPARO_INFERENCE_URL` — **required**. Provider base URL
    ///   (`http://localhost:11434/v1`, `https://api.openai.com/v1`,
    ///   `https://api.anthropic.com`, …).
    /// - `AMPARO_INFERENCE_KEY` — API key (empty for keyless local providers).
    /// - `AMPARO_INFERENCE_MODEL` — **required**. Default model ID.
    /// - `AMPARO_INFERENCE_PROVIDER` — a provider catalog id or alias
    ///   (default `openai`): `anthropic`, `openai`, `moonshot` (`kimi`),
    ///   `deepseek`, `qwen`, `glm`, `mistral`, `groq`, `openrouter`,
    ///   `gemini`, `ollama`, `lmstudio`, or `custom`. An unknown id fails
    ///   with the full list instead of guessing a wire protocol.
    /// - `AMPARO_INFERENCE_TIMEOUT_SECS` — request timeout (default 120,
    ///   clamped to 1–3600).
    /// - `AMPARO_INFERENCE_MAX_TOKENS` — optional per-request `max_tokens` cap.
    /// - `AMPARO_INFERENCE_MODEL_ALLOWLIST` — optional comma-separated list;
    ///   when set, only listed models may run.
    ///
    /// Fails with `InferenceError::Config` rather than falling back to any
    /// hardcoded default.
    pub fn from_env() -> Result<Self> {
        let base_url = std::env::var("AMPARO_INFERENCE_URL").map_err(|_| {
            InferenceError::Config(
                "AMPARO_INFERENCE_URL is required (e.g. http://localhost:11434/v1 \
                 or https://api.anthropic.com)"
                    .to_string(),
            )
        })?;
        let model = std::env::var("AMPARO_INFERENCE_MODEL").map_err(|_| {
            InferenceError::Config(
                "AMPARO_INFERENCE_MODEL is required (e.g. qwen2.5:14b or \
                 claude-sonnet-5-20250929)"
                    .to_string(),
            )
        })?;
        let provider_raw = std::env::var("AMPARO_INFERENCE_PROVIDER")
            .unwrap_or_else(|_| "openai".to_string());
        let spec = catalog::lookup(&provider_raw).ok_or_else(|| {
            InferenceError::Config(format!(
                "unknown provider '{}' — known providers: {}",
                provider_raw.trim(),
                catalog::known_ids().join(", ")
            ))
        })?;
        let provider = spec.wire;
        let provider_id = Some(spec.id.to_string());
        let timeout_secs = std::env::var("AMPARO_INFERENCE_TIMEOUT_SECS")
            .ok()
            .map(|v| {
                v.parse::<u64>().map_err(|_| {
                    InferenceError::Config(format!(
                        "AMPARO_INFERENCE_TIMEOUT_SECS must be an integer, got '{}'",
                        v
                    ))
                })
            })
            .transpose()?
            .unwrap_or(DEFAULT_TIMEOUT_SECS)
            .clamp(1, MAX_TIMEOUT_SECS);
        let max_tokens_limit = std::env::var("AMPARO_INFERENCE_MAX_TOKENS")
            .ok()
            .map(|v| {
                v.parse::<usize>().map_err(|_| {
                    InferenceError::Config(format!(
                        "AMPARO_INFERENCE_MAX_TOKENS must be an integer, got '{}'",
                        v
                    ))
                })
            })
            .transpose()?;
        let allowlist = std::env::var("AMPARO_INFERENCE_MODEL_ALLOWLIST")
            .ok()
            .map(|v| {
                let models: Vec<String> = v
                    .split(',')
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect();
                if models.is_empty() {
                    Err(InferenceError::Config(
                        "AMPARO_INFERENCE_MODEL_ALLOWLIST is set but empty".to_string(),
                    ))
                } else {
                    Ok(models)
                }
            })
            .transpose()?;

        Ok(Self {
            base_url,
            api_key: std::env::var("AMPARO_INFERENCE_KEY").unwrap_or_default(),
            model,
            provider,
            provider_id,
            timeout_secs,
            max_tokens_limit,
            allowlist,
            cloud: CloudConfig::from_env(),
        })
    }

    /// Resolve the catalog spec driving this config's behavior flags:
    /// the `provider_id` when set, else the kind's canonical entry.
    fn resolved_spec(&self) -> Option<&'static catalog::ProviderSpec> {
        if let Some(id) = &self.provider_id {
            return catalog::lookup(id);
        }
        match &self.provider {
            ProviderKind::OpenAI => catalog::lookup("openai"),
            ProviderKind::Anthropic => catalog::lookup("anthropic"),
            ProviderKind::OllamaNative => catalog::lookup("ollama"),
        }
    }

    /// Instantiate a concrete provider from this config.
    ///
    /// Fails with `InferenceError::Config` if the configured model is not on
    /// the allowlist (when one is set).
    pub fn build(&self) -> Result<Arc<dyn InferenceProvider>> {
        if let Some(allowlist) = &self.allowlist {
            if !allowlist.iter().any(|m| m == &self.model) {
                return Err(InferenceError::Config(format!(
                    "model '{}' is not in AMPARO_INFERENCE_MODEL_ALLOWLIST",
                    self.model
                )));
            }
        }
        // Behavior flags come from the resolved catalog entry; an unknown
        // provider id fails here (configuration time), not on the first
        // request.
        let spec = self.resolved_spec().ok_or_else(|| {
            InferenceError::Config(format!(
                "unknown provider id '{}' — known providers: {}",
                self.provider_id.as_deref().unwrap_or_default(),
                catalog::known_ids().join(", ")
            ))
        })?;
        match &self.provider {
            ProviderKind::OpenAI => {
                let base_url =
                    catalog::append_v1_if_bare(&catalog::normalize_base_url(&self.base_url)?);
                Ok(Arc::new(OpenAIProvider::new(
                    base_url,
                    self.api_key.clone(),
                    self.model.clone(),
                    self.timeout_secs,
                    self.max_tokens_limit,
                    ProviderKind::OpenAI,
                    spec.reasoning,
                )))
            }
            ProviderKind::Anthropic => {
                // Validate the URL but leave shaping to AnthropicProvider
                // (it accepts both `.../v1` and bare-host forms).
                let base_url = catalog::normalize_base_url(&self.base_url)?;
                Ok(Arc::new(AnthropicProvider::new(
                    base_url,
                    self.api_key.clone(),
                    self.model.clone(),
                    self.timeout_secs,
                    self.max_tokens_limit,
                )))
            }
            ProviderKind::OllamaNative => {
                // Ollama's native /api/chat — the base host, no /v1.
                let base_url = catalog::normalize_base_url(&self.base_url)?;
                Ok(Arc::new(OpenAIProvider::new(
                    base_url,
                    self.api_key.clone(),
                    self.model.clone(),
                    self.timeout_secs,
                    self.max_tokens_limit,
                    ProviderKind::OllamaNative,
                    spec.reasoning,
                )))
            }
        }
    }

    /// Build a provider with cloud fallback if configured.
    ///
    /// Returns a `FallbackProvider` wrapping primary → cloud when cloud is
    /// available, or just the primary provider when it's not.
    pub fn build_with_fallback(&self) -> Result<Arc<dyn InferenceProvider>> {
        let primary = self.build()?;
        match self.cloud.build_provider() {
            Some(cloud) => Ok(Arc::new(FallbackProvider::new(primary, cloud))),
            None => Ok(primary),
        }
    }
}

// ─────────────────────────────────────────────────── InferenceHandle ─────────

/// A live, hot-swappable provider wrapper.  `Arc<InferenceHandle>` can be
/// coerced to `Arc<dyn InferenceProvider>` and passed anywhere that expects an
/// inference backend — while still allowing runtime reconfiguration via
/// `reconfigure()` without restarting the daemon.
pub struct InferenceHandle {
    inner:  tokio::sync::RwLock<Arc<dyn InferenceProvider>>,
    /// Use std::sync so `default_model()` (sync trait method) can read it.
    config: std::sync::RwLock<InferenceConfig>,
}

impl InferenceHandle {
    /// Create a handle from a config, building the inner provider via `build()`
    /// and failing closed on invalid configuration.
    pub fn new(config: InferenceConfig) -> Result<Arc<Self>> {
        let provider = config.build()?;
        Ok(Arc::new(Self {
            inner:  tokio::sync::RwLock::new(provider),
            config: std::sync::RwLock::new(config),
        }))
    }

    /// Create an `InferenceHandle` with cloud fallback enabled.
    ///
    /// Builds a `FallbackProvider` chain (primary → cloud) when cloud is
    /// configured, otherwise falls back to primary-only. This is the preferred
    /// constructor for daemon startup.
    pub fn new_with_fallback(config: InferenceConfig) -> Result<Arc<Self>> {
        let provider = config.build_with_fallback()?;
        Ok(Arc::new(Self {
            inner:  tokio::sync::RwLock::new(provider),
            config: std::sync::RwLock::new(config),
        }))
    }

    /// Snapshot of the current configuration (api_key is the live value).
    pub fn get_config(&self) -> InferenceConfig {
        self.config.read().unwrap().clone()
    }

    /// Atomically swap to a new provider built from `new_cfg`.
    /// All in-flight requests on the old provider complete normally.
    pub async fn reconfigure(&self, new_cfg: InferenceConfig) -> Result<()> {
        let new_provider = new_cfg.build()?;
        *self.inner.write().await = new_provider;
        *self.config.write().unwrap() = new_cfg;
        Ok(())
    }

    /// Replace the inner provider with a pre-built one, updating config.
    /// Useful for wrapping the provider (e.g. FallbackProvider) outside of
    /// the normal build path.
    pub async fn set_provider(&self, provider: Arc<dyn InferenceProvider>, cfg: InferenceConfig) {
        *self.inner.write().await = provider;
        *self.config.write().unwrap() = cfg;
    }

    /// Get a clone of the current inner provider, e.g. to wrap as a fallback.
    pub async fn get_provider(&self) -> Arc<dyn InferenceProvider> {
        Arc::clone(&*self.inner.read().await)
    }
}

#[async_trait]
impl InferenceProvider for InferenceHandle {
    async fn complete(&self, request: InferenceRequest) -> Result<InferenceResponse> {
        self.inner.read().await.complete(request).await
    }
    async fn embed(&self, text: &str) -> Result<Vec<f64>> {
        self.inner.read().await.embed(text).await
    }
    async fn list_models(&self) -> Result<Vec<String>> {
        self.inner.read().await.list_models().await
    }
    fn default_model(&self) -> String {
        self.config.read().unwrap().model.clone()
    }
    async fn complete_chat_stream(&self, request: ChatRequest) -> Result<InferenceStream> {
        self.inner.read().await.complete_chat_stream(request).await
    }
}

// ──────────────────────────────────────────── FallbackProvider ────────────────

/// Wraps a primary and fallback provider. If the primary fails, the fallback is
/// tried transparently. Used to make the bundled local model the primary while
/// keeping Ollama as a fallback for graceful degradation.
pub struct FallbackProvider {
    primary: Arc<dyn InferenceProvider>,
    fallback: Arc<dyn InferenceProvider>,
}

impl FallbackProvider {
    /// Create a provider that tries `primary` first and `fallback` on failure.
    pub fn new(
        primary: Arc<dyn InferenceProvider>,
        fallback: Arc<dyn InferenceProvider>,
    ) -> Self {
        Self { primary, fallback }
    }
}

#[async_trait]
impl InferenceProvider for FallbackProvider {
    async fn complete(&self, request: InferenceRequest) -> Result<InferenceResponse> {
        match self.primary.complete(request.clone()).await {
            Ok(resp) => Ok(resp),
            Err(_) => self.fallback.complete(request).await,
        }
    }

    async fn complete_chat_stream(&self, request: ChatRequest) -> Result<InferenceStream> {
        match self.primary.complete_chat_stream(request.clone()).await {
            Ok(stream) => Ok(stream),
            Err(_) => self.fallback.complete_chat_stream(request).await,
        }
    }

    async fn embed(&self, text: &str) -> Result<Vec<f64>> {
        self.primary.embed(text).await
    }

    async fn list_models(&self) -> Result<Vec<String>> {
        self.primary.list_models().await
    }

    fn default_model(&self) -> String {
        self.primary.default_model()
    }
}

// ──────────────────────────────────────────── Local model probe helper ────────

/// Probe any OpenAI-compatible endpoint for available model IDs.
/// Returns an empty vec on any error (server down, timeout, etc.).
/// Useful for auto-discovering Ollama models.
pub async fn probe_local_models(base_url: &str) -> Vec<String> {
    let client = match reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .build()
    {
        Ok(c) => c,
        Err(_) => return Vec::new(),
    };

    let url = format!("{}/models", base_url.trim_end_matches('/'));
    match client.get(&url).send().await {
        Ok(resp) if resp.status().is_success() => {
            let payload: serde_json::Value =
                resp.json().await.unwrap_or(serde_json::Value::Null);
            OpenAIProvider::parse_model_ids_pub(&payload)
        }
        _ => Vec::new(),
    }
}

/// Inference response
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InferenceResponse {
    /// The generated response text.
    pub text: String,
    /// Total tokens reported by the provider for this request.
    pub tokens: usize,
    /// Why generation stopped (`stop`, `length`, `tool_calls`, ...).
    pub finish_reason: String,
}

/// A function call made by the assistant (OpenAI tool-call shape).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct FunctionCall {
    /// The name of the function the model wants to call.
    pub name: String,
    /// Arguments as a JSON string (OpenAI convention; parsed into an object
    /// when translated to the Anthropic Messages API).
    pub arguments: String,
}

/// One entry in an assistant message's `tool_calls` array.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AssistantToolCall {
    /// Unique ID for this tool call, echoed back in the answering tool result.
    pub id: String,
    /// Tool call type; always `"function"`.
    #[serde(rename = "type")]
    pub call_type: String, // always "function"
    /// The function being called, with JSON-encoded arguments.
    pub function: FunctionCall,
}

/// A single message in a chat conversation (OpenAI-compatible).
///
/// Enriched beyond the original Axiom-OS shape so that native tool calling
/// round-trips through the wire format: assistant messages may carry
/// `tool_calls`, and tool-role messages carry the `tool_call_id` they answer.
/// Both optional fields are skipped when absent, so messages built the old
/// way serialize exactly as before.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatMessage {
    /// Message role: `"system"`, `"user"`, `"assistant"`, or `"tool"`.
    pub role: String,       // "system" | "user" | "assistant" | "tool"
    /// Message body text; empty for tool-call-only assistant messages.
    pub content: String,
    /// Native tool calls made by the assistant (assistant messages only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<AssistantToolCall>>,
    /// Which assistant tool call this message answers (tool messages only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    /// Reasoning trace the provider returned for an assistant turn
    /// (Moonshot/Kimi K3, DeepSeek, …). Echoed back verbatim on the next
    /// request when the provider's catalog entry says `reasoning: true` —
    /// those endpoints break multi-turn tool calling without it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_content: Option<String>,
}

impl ChatMessage {
    /// Build a `"user"` role message.
    pub fn user(content: impl Into<String>) -> Self {
        Self {
            role: "user".into(),
            content: content.into(),
            tool_calls: None,
            tool_call_id: None,
            reasoning_content: None,
        }
    }
    /// Build an `"assistant"` role message.
    pub fn assistant(content: impl Into<String>) -> Self {
        Self {
            role: "assistant".into(),
            content: content.into(),
            tool_calls: None,
            tool_call_id: None,
            reasoning_content: None,
        }
    }
    /// Build a `"system"` role message.
    pub fn system(content: impl Into<String>) -> Self {
        Self {
            role: "system".into(),
            content: content.into(),
            tool_calls: None,
            tool_call_id: None,
            reasoning_content: None,
        }
    }
    /// A tool result answering the assistant tool call with `call_id`.
    pub fn tool(call_id: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            role: "tool".into(),
            content: content.into(),
            tool_calls: None,
            tool_call_id: Some(call_id.into()),
            reasoning_content: None,
        }
    }
    /// An assistant message that only makes tool calls (no prose).
    pub fn assistant_tool_calls(calls: Vec<AssistantToolCall>) -> Self {
        Self {
            role: "assistant".into(),
            content: String::new(),
            tool_calls: Some(calls),
            tool_call_id: None,
            reasoning_content: None,
        }
    }
}

/// OpenAI-compatible tool definition
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Tool {
    /// Tool type; always `"function"`.
    #[serde(rename = "type")]
    pub tool_type: String,  // always "function"
    /// Function definition: `{ name, description, parameters }`.
    pub function: serde_json::Value,  // { name, description, parameters }
}

/// Chat inference request (messages + optional tools)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatRequest {
    /// The conversation history, oldest first.
    pub messages: Vec<ChatMessage>,
    /// Optional tool definitions made available to the model.
    pub tools: Option<Vec<Tool>>,
    /// Maximum tokens for the response, clamped to the provider limit when set.
    pub max_tokens: Option<usize>,
    /// Sampling temperature; defaults to 0.7 when unset.
    pub temperature: Option<f32>,
    /// Whether the response is requested as a stream.
    pub stream: Option<bool>,
    /// Override the provider's default model for this request.
    pub model: Option<String>,
    /// Privacy level — determines routing.
    #[serde(default)]
    pub privacy_level: Option<String>,
    /// Structured output JSON schema.
    #[serde(default)]
    pub json_schema: Option<serde_json::Value>,
    /// Thinking mode control for reasoning models.
    #[serde(default)]
    pub thinking: Option<String>,
    /// Enable web search (Ollama 0.18+).
    #[serde(default)]
    pub web_search: Option<bool>,
    /// Challenge gradient tier (1-3) — controls response creativity/safety.
    #[serde(default)]
    pub challenge_level: Option<u8>,
}

/// Inference provider trait
#[async_trait]
pub trait InferenceProvider: Send + Sync {
    /// Run a non-streaming completion and return the full response.
    async fn complete(&self, request: InferenceRequest) -> Result<InferenceResponse>;
    /// Embed text into a vector; providers without an embeddings API fail with
    /// `InvalidRequest`.
    async fn embed(&self, text: &str) -> Result<Vec<f64>>;
    /// List the model IDs the provider can serve, sorted and deduplicated.
    async fn list_models(&self) -> Result<Vec<String>>;
    /// The provider's default model ID.
    fn default_model(&self) -> String;
    /// Chat completions with optional tool definitions. Returns a byte stream of SSE events.
    async fn complete_chat_stream(
        &self,
        request: ChatRequest,
    ) -> Result<InferenceStream>;
}

/// OpenAI-compatible provider
pub struct OpenAIProvider {
    client: reqwest::Client,
    base_url: String,
    api_key: String,
    model: String,
    /// Configurable embedding model name (e.g., "nomic-embed-text" for local, "text-embedding-3-small" for cloud)
    embedding_model: String,
    /// Per-request timeout in seconds (connect + headers; the response body
    /// stream additionally gets an idle timeout between items).
    timeout_secs: u64,
    /// Upper bound on `max_tokens`; requests are clamped to it.
    max_tokens_limit: Option<usize>,
    /// Which wire to speak: OpenAI-compatible `/chat/completions`, or
    /// Ollama's native `/api/chat` (any host, not just the localhost sniff).
    wire: ProviderKind,
    /// Echo `reasoning_content` on assistant turns (catalog `reasoning` flag).
    reasoning: bool,
}

impl OpenAIProvider {
    /// Create an OpenAI-compatible provider. The embedding model is read from
    /// `AMPARO_INFERENCE_EMBEDDING_MODEL` (default `nomic-embed-text`).
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        base_url: String,
        api_key: String,
        model: String,
        timeout_secs: u64,
        max_tokens_limit: Option<usize>,
        wire: ProviderKind,
        reasoning: bool,
    ) -> Self {
        let embedding_model = std::env::var("AMPARO_INFERENCE_EMBEDDING_MODEL")
            .unwrap_or_else(|_| "nomic-embed-text".to_string());
        let client = reqwest::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(10))
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());
        Self {
            client,
            base_url,
            api_key,
            model,
            embedding_model,
            timeout_secs,
            max_tokens_limit,
            wire,
            reasoning,
        }
    }

    fn timeout_dur(&self) -> std::time::Duration {
        std::time::Duration::from_secs(self.timeout_secs)
    }

    /// Send with a hard timeout covering connection, TLS, and headers.
    async fn send_with_timeout(
        &self,
        req: reqwest::RequestBuilder,
    ) -> Result<reqwest::Response> {
        tokio::time::timeout(self.timeout_dur(), req.send())
            .await
            .map_err(|_| InferenceError::Timeout)?
            .map_err(|e| InferenceError::Provider(e.to_string()))
    }

    /// Read a JSON body with a hard timeout.
    async fn response_json(&self, resp: reqwest::Response) -> Result<serde_json::Value> {
        tokio::time::timeout(self.timeout_dur(), resp.json())
            .await
            .map_err(|_| InferenceError::Timeout)?
            .map_err(|e| InferenceError::Provider(e.to_string()))
    }

    /// Create with explicit embedding model override.
    pub fn with_embedding_model(mut self, embedding_model: String) -> Self {
        self.embedding_model = embedding_model;
        self
    }

    /// Public alias used by `probe_local_models`.
    pub fn parse_model_ids_pub(payload: &serde_json::Value) -> Vec<String> {
        Self::parse_model_ids(payload)
    }

    fn parse_model_ids(payload: &serde_json::Value) -> Vec<String> {
        let mut model_ids: Vec<String> = Vec::new();

        if let Some(items) = payload.get("data").and_then(|v| v.as_array()) {
            for item in items {
                if let Some(id) = item.get("id").and_then(|v| v.as_str()) {
                    model_ids.push(id.to_string());
                }
            }
        }

        if model_ids.is_empty() {
            if let Some(items) = payload.get("models").and_then(|v| v.as_array()) {
                for item in items {
                    if let Some(id) = item.as_str() {
                        model_ids.push(id.to_string());
                        continue;
                    }
                    if let Some(id) = item.get("id").and_then(|v| v.as_str()) {
                        model_ids.push(id.to_string());
                        continue;
                    }
                    if let Some(name) = item.get("name").and_then(|v| v.as_str()) {
                        model_ids.push(name.to_string());
                    }
                }
            }
        }

        model_ids.sort();
        model_ids.dedup();
        model_ids
    }

    /// Embed text with a specific model override.
    pub async fn embed_with_model(&self, text: &str, model: &str) -> Result<Vec<f64>> {
        let req = self
            .client
            .post(format!("{}/embeddings", self.base_url))
            .header("Authorization", format!("Bearer {}", self.api_key))
            .header("Content-Type", "application/json")
            .json(&serde_json::json!({
                "model": model,
                "input": text
            }));
        let response = self.send_with_timeout(req).await?;

        let data: serde_json::Value = self.response_json(response).await?;

        let embedding = data["data"][0]["embedding"]
            .as_array()
            .unwrap_or(&Vec::new())
            .iter()
            .map(|v| v.as_f64().unwrap_or(0.0))
            .collect();

        Ok(embedding)
    }

    /// Use Ollama's native `/api/chat` endpoint for streaming.
    /// This endpoint correctly respects `"think": false` (unlike `/v1/chat/completions`).
    /// Returns an `InferenceStream` that emits SSE-formatted bytes so the caller
    /// (daemon → frontend) doesn't need to know about the format difference.
    async fn complete_chat_stream_ollama_native(
        &self,
        model: &str,
        request: &ChatRequest,
        temperature: f32,
        think: bool,
    ) -> Result<InferenceStream> {
        // Ollama native /api/chat uses the base host, not /v1.
        let ollama_base = self
            .base_url
            .replace("/v1", "")
            .trim_end_matches('/')
            .to_string();

        let mut body = serde_json::json!({
            "model": model,
            "messages": request.messages,
            "stream": true,
            "think": think,
            "options": {
                "temperature": temperature,
                "num_predict": clamp_max_tokens(request.max_tokens, self.max_tokens_limit)
            }
        });

        // Tools passthrough
        if let Some(tools) = &request.tools {
            if !tools.is_empty() {
                body["tools"] = serde_json::to_value(tools)
                    .map_err(|e| InferenceError::Provider(e.to_string()))?;
            }
        }

        // Structured output
        if let Some(schema) = &request.json_schema {
            body["format"] = schema.clone();
        }

        let req = self
            .client
            .post(format!("{}/api/chat", ollama_base))
            .header("Content-Type", "application/json")
            .json(&body);
        let resp = self.send_with_timeout(req).await?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            return Err(InferenceError::Provider(format!(
                "Ollama /api/chat failed (HTTP {}): {}",
                status, text
            )));
        }

        // Ollama native streaming returns newline-delimited JSON:
        //   {"message":{"role":"assistant","content":"Hello"},"done":false}\n
        //   {"message":{"role":"assistant","content":"!"},"done":false}\n
        //   {"done":true,"total_duration":...}\n
        //
        // Convert to SSE format that the daemon/frontend expects:
        //   data: {"choices":[{"delta":{"content":"Hello"}}]}\n\n
        //   data: {"choices":[{"delta":{"content":"!"}}]}\n\n
        //   data: [DONE]\n\n

        use futures_util::StreamExt;

        let stream = resp.bytes_stream();
        let sse_stream = {
            let mut line_buf = String::new();
            stream.flat_map(move |chunk_result| {
                let mut sse_events: Vec<std::result::Result<bytes::Bytes, InferenceError>> =
                    Vec::new();
                match chunk_result {
                    Err(e) => {
                        sse_events
                            .push(Err(InferenceError::Provider(e.to_string())));
                    }
                    Ok(chunk) => {
                        let text = String::from_utf8_lossy(&chunk);
                        line_buf.push_str(&text);

                        while let Some(newline_pos) = line_buf.find('\n') {
                            let line: String = line_buf.drain(..=newline_pos).collect();
                            let line = line.trim();
                            if line.is_empty() {
                                continue;
                            }

                            if let Ok(obj) =
                                serde_json::from_str::<serde_json::Value>(line)
                            {
                                if obj.get("done") == Some(&serde_json::json!(true)) {
                                    let done_bytes =
                                        bytes::Bytes::from("data: [DONE]\n\n");
                                    sse_events.push(Ok(done_bytes));
                                } else if let Some(msg) = obj.get("message") {
                                    let content = msg
                                        .get("content")
                                        .and_then(|v| v.as_str())
                                        .unwrap_or("");
                                    let sse_payload = serde_json::json!({
                                        "choices": [{
                                            "delta": { "content": content }
                                        }]
                                    });
                                    let sse_line = format!(
                                        "data: {}\n\n",
                                        serde_json::to_string(&sse_payload)
                                            .unwrap_or_default()
                                    );
                                    sse_events
                                        .push(Ok(bytes::Bytes::from(sse_line)));
                                }
                            }
                        }
                    }
                }
                futures_util::stream::iter(sse_events)
            })
        };

        Ok(stream_with_idle_timeout(sse_stream, self.timeout_dur()))
    }
}

#[async_trait]
impl InferenceProvider for OpenAIProvider {
    async fn complete(&self, request: InferenceRequest) -> Result<InferenceResponse> {
        let model = request.model.as_deref().unwrap_or(&self.model);
        let think_enabled = request.thinking.as_ref()
            .map(|t| !matches!(t.to_lowercase().as_str(), "off" | "false" | "no" | "0" | "disabled"))
            .unwrap_or(false);

        // Legacy localhost sniff kept for pre-catalog configs; the catalog
        // `ollama` entry forces native /api/chat for any host.
        let is_local_ollama = self.wire == ProviderKind::OllamaNative
            || self.base_url.contains("localhost:11434")
            || self.base_url.contains("127.0.0.1:11434");

        let data: serde_json::Value = if is_local_ollama {
            // Native Ollama /api/chat — respects think: false
            let ollama_base = self.base_url.replace("/v1", "").trim_end_matches('/').to_string();
            let req = self.client
                .post(format!("{}/api/chat", ollama_base))
                .header("Content-Type", "application/json")
                .json(&serde_json::json!({
                    "model": model,
                    "messages": [{"role": "user", "content": request.prompt}],
                    "stream": false,
                    "think": think_enabled,
                    "options": {
                        "temperature": request.temperature.unwrap_or(0.7),
                        "num_predict": clamp_max_tokens(request.max_tokens, self.max_tokens_limit)
                    }
                }));
            let response = self.send_with_timeout(req).await?;
            if !response.status().is_success() {
                let status = response.status();
                let text = response.text().await.unwrap_or_default();
                return Err(InferenceError::Provider(format!(
                    "Ollama /api/chat failed (HTTP {}): {}",
                    status, text
                )));
            }
            let native: serde_json::Value = self.response_json(response).await?;
            // Normalize native response to OpenAI shape for uniform extraction below
            serde_json::json!({
                "choices": [{
                    "message": { "content": native["message"]["content"].as_str().unwrap_or("") },
                    "finish_reason": if native["done"].as_bool() == Some(true) { "stop" } else { "length" }
                }],
                "usage": {
                    "total_tokens": native["eval_count"].as_u64().unwrap_or(0)
                        + native["prompt_eval_count"].as_u64().unwrap_or(0)
                }
            })
        } else {
            // Standard OpenAI-compatible path
            let req = self.client
                .post(format!("{}/chat/completions", self.base_url))
                .header("Authorization", format!("Bearer {}", self.api_key))
                .header("Content-Type", "application/json")
                .json(&serde_json::json!({
                    "model": model,
                    "messages": [{"role": "user", "content": request.prompt}],
                    "max_tokens": clamp_max_tokens(request.max_tokens, self.max_tokens_limit),
                    "temperature": request.temperature.unwrap_or(0.7)
                }));
            let response = self.send_with_timeout(req).await?;
            if !response.status().is_success() {
                let status = response.status();
                let text = response.text().await.unwrap_or_default();
                return Err(InferenceError::Provider(format!(
                    "chat/completions failed (HTTP {}): {}",
                    status, text
                )));
            }
            self.response_json(response).await?
        };

        let text = data["choices"][0]["message"]["content"]
            .as_str()
            .unwrap_or("")
            .to_string();

        let tokens = data["usage"]["total_tokens"].as_u64().unwrap_or(0) as usize;
        let finish_reason = data["choices"][0]["finish_reason"]
            .as_str()
            .unwrap_or("stop")
            .to_string();

        Ok(InferenceResponse {
            text,
            tokens,
            finish_reason,
        })
    }

    async fn complete_chat_stream(
        &self,
        mut request: ChatRequest,
    ) -> Result<InferenceStream> {
        let model = request.model.as_deref().unwrap_or(&self.model);
        let base_temp = request.temperature.unwrap_or(0.7);
        let effective_temp = temperature_from_challenge(request.challenge_level, base_temp);

        // Thinking mode for reasoning models — default OFF for CPU performance.
        let think_enabled = request.thinking.as_ref()
            .map(|t| !matches!(t.to_lowercase().as_str(), "off" | "false" | "no" | "0" | "disabled"))
            .unwrap_or(false);

        // ── Detect local Ollama and use native /api/chat ────────────────
        // Ollama's OpenAI-compatible /v1/chat/completions endpoint ignores
        // the `think` parameter.  Only the native /api/chat respects it.
        // The catalog `ollama` entry forces native for any host; the
        // localhost sniff keeps pre-catalog configs working.
        let is_local_ollama = self.wire == ProviderKind::OllamaNative
            || self.base_url.contains("localhost:11434")
            || self.base_url.contains("127.0.0.1:11434");

        if is_local_ollama {
            return self
                .complete_chat_stream_ollama_native(
                    model,
                    &request,
                    effective_temp,
                    think_enabled,
                )
                .await;
        }

        // ── Standard OpenAI-compatible path ─────────────────────────────
        // Strict endpoints (Moonshot/Kimi, DeepSeek, …) 400 on unknown
        // fields, so the compat body carries only OpenAI-shaped keys:
        // `think`, `format`, and `web_search` never leave via this path.
        // `reasoning_content` is echoed on assistant turns only when the
        // resolved provider spec says so.
        if !self.reasoning {
            for msg in &mut request.messages {
                if msg.role == "assistant" {
                    msg.reasoning_content = None;
                }
            }
        }

        let mut body = serde_json::json!({
            "model": model,
            "messages": request.messages,
            "stream": true,
            "max_tokens": clamp_max_tokens(request.max_tokens, self.max_tokens_limit),
            "temperature": effective_temp
        });

        if let Some(tools) = &request.tools {
            if !tools.is_empty() {
                body["tools"] = serde_json::to_value(tools)
                    .map_err(|e| InferenceError::Provider(e.to_string()))?;
            }
        }

        if let Some(schema) = &request.json_schema {
            body["response_format"] = serde_json::json!({
                "type": "json_schema",
                "json_schema": {
                    "name": "amparo_output",
                    "schema": schema,
                }
            });
        }

        let req = self
            .client
            .post(format!("{}/chat/completions", self.base_url))
            .header("Authorization", format!("Bearer {}", self.api_key))
            .header("Content-Type", "application/json")
            .json(&body);
        let resp = self.send_with_timeout(req).await?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            return Err(InferenceError::Provider(format!(
                "chat/completions failed (HTTP {}): {}",
                status, text
            )));
        }

        // Wrap reqwest byte stream into our InferenceStream type, with an
        // idle timeout between items.
        use futures_util::StreamExt;
        let stream = resp.bytes_stream().map(|result| {
            result.map_err(|e| InferenceError::Provider(e.to_string()))
        });
        Ok(stream_with_idle_timeout(stream, self.timeout_dur()))
    }

    async fn list_models(&self) -> Result<Vec<String>> {
        let url = format!("{}/models", self.base_url.trim_end_matches('/'));
        let mut req = self
            .client
            .get(url)
            .header("Content-Type", "application/json");

        if !self.api_key.is_empty() {
            req = req.header("Authorization", format!("Bearer {}", self.api_key));
        }

        let response = self.send_with_timeout(req).await?;

        if !response.status().is_success() {
            return Err(InferenceError::Provider(format!(
                "model list request failed: HTTP {}",
                response.status()
            )));
        }

        let payload: serde_json::Value = self.response_json(response).await?;

        let mut model_ids = Self::parse_model_ids(&payload);
        if model_ids.is_empty() {
            model_ids.push(self.model.clone());
        }
        Ok(model_ids)
    }

    fn default_model(&self) -> String {
        self.model.clone()
    }

    async fn embed(&self, text: &str) -> Result<Vec<f64>> {
        let req = self
            .client
            .post(format!("{}/embeddings", self.base_url))
            .header("Authorization", format!("Bearer {}", self.api_key))
            .header("Content-Type", "application/json")
            .json(&serde_json::json!({
                "model": self.embedding_model,
                "input": text
            }));
        let response = self.send_with_timeout(req).await?;
        if !response.status().is_success() {
            let status = response.status();
            let text = response.text().await.unwrap_or_default();
            return Err(InferenceError::Provider(format!(
                "embeddings failed (HTTP {}): {}",
                status, text
            )));
        }

        let data: serde_json::Value = self.response_json(response).await?;

        let embedding = data["data"][0]["embedding"]
            .as_array()
            .unwrap_or(&Vec::new())
            .iter()
            .map(|v| v.as_f64().unwrap_or(0.0))
            .collect();

        Ok(embedding)
    }
}

// ───────────────────────────────── Cloud model helpers ────────────────────────

/// Returns `true` if the model ID uses Ollama's `:cloud` tag,
/// meaning inference runs on a remote provider rather than locally.
pub fn is_cloud_model(model: &str) -> bool {
    model.ends_with(":cloud") || model.contains(":cloud-")
}

/// Returns true if the model is an embedding-only model that cannot handle chat.
/// These models must never be selected for chat/completion requests.
pub fn is_embedding_model(model: &str) -> bool {
    let lower = model.to_lowercase();
    lower.contains("embed") || lower.contains("nomic") || lower.starts_with("e5-")
}

/// Recommend a model based on task complexity and privacy level.
/// Returns (model_id, is_cloud).
pub fn recommend_model(
    task_hint: &str,
    privacy_level: Option<&str>,
    available_models: &[String],
) -> (String, bool) {
    let is_strict_local = privacy_level == Some("strict_local");

    // Filter out cloud models if strict_local, and always exclude embedding-only models.
    let candidates: Vec<&String> = if is_strict_local {
        available_models
            .iter()
            .filter(|m| !is_cloud_model(m) && !is_embedding_model(m))
            .collect()
    } else {
        available_models
            .iter()
            .filter(|m| !is_embedding_model(m))
            .collect()
    };

    // Simple heuristic: prefer larger models for complex tasks
    let complex = task_hint.contains("research")
        || task_hint.contains("analysis")
        || task_hint.contains("code")
        || task_hint.contains("reason");

    if candidates.is_empty() {
        return ("llama3".to_string(), false);
    }

    // Prefer cloud for complex tasks when allowed
    if complex && !is_strict_local {
        if let Some(cloud) = candidates.iter().find(|m| is_cloud_model(m)) {
            return (cloud.to_string(), true);
        }
    }

    (candidates[0].to_string(), is_cloud_model(candidates[0]))
}

/// Calculate temperature from challenge gradient level.
/// Level 1 (Conservative): Low temperature = focused, safe responses
/// Level 2 (Balanced): Medium temperature = default behavior  
/// Level 3 (Creative): High temperature = more creative, varied responses
pub fn temperature_from_challenge(level: Option<u8>, default: f32) -> f32 {
    match level {
        Some(1) => 0.3,  // Conservative - focused, deterministic
        Some(2) => 0.6,  // Balanced
        Some(3) => 0.9,  // Creative - varied, exploratory
        Some(l) if l > 3 => 0.9,  // Cap at creative
        _ => default,    // Use provided default
    }
}

// ──────────────────────────────────── Task Planner ───────────────────────────

/// System prompt used for the planning decomposition turn.
const PLANNER_SYSTEM: &str = "\
You are a task planner. Break the user's request into a numbered list of \
concrete, independent steps. Be concise — one line per step. \
Do NOT execute anything. Output ONLY the numbered list, nothing else.";

/// For tasks above this complexity, prepend a planning decomposition turn.
pub const PLAN_COMPLEXITY_THRESHOLD: f64 = 0.5;

/// Build the prompt text for a planning turn.
/// Returns `None` if the task is simple enough to skip planning.
pub fn planning_prompt(task: &str) -> Option<String> {
    if analyze_task_complexity(task) >= PLAN_COMPLEXITY_THRESHOLD {
        Some(format!(
            "[system]\n{}\n\n[user]\n{}",
            PLANNER_SYSTEM, task
        ))
    } else {
        None
    }
}

// ──────────────────────────────────── Model Router ────────────────────────────
/// Routes requests to the best available model based on task complexity,
/// privacy requirements, and available models. Supports local-first routing
/// with cloud fallback for complex tasks.

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelRoute {
    /// The selected model ID.
    pub model: String,
    /// Whether the route runs inference on a remote (cloud) provider.
    pub is_cloud: bool,
    /// Human-readable explanation of why this route was chosen.
    pub reasoning: String,
    /// Task complexity score (0.0–1.0) that drove the routing decision.
    pub complexity_score: f64,
}

/// Analyze task complexity from the prompt text.
/// Returns a score from 0.0 (trivial) to 1.0 (highly complex).
pub fn analyze_task_complexity(prompt: &str) -> f64 {
    analyze_task_complexity_with_coherence(prompt, None)
}

/// Coherence signal for model routing decisions.
/// When available, adjusts complexity scoring based on the companion's emotional state.
#[derive(Debug, Clone)]
pub struct CoherenceSignal {
    /// Current valence (-1.0 to 1.0). Negative = user seems frustrated/stressed.
    pub current_valence: f64,
    /// Drift from baseline (0.0 = stable, higher = more drift).
    pub drift_score: f64,
}

/// Analyze task complexity with optional coherence awareness (Improvement #6).
///
/// When the user is experiencing high drift or negative valence, the system
/// should prefer more capable (often cloud) models to provide better support.
pub fn analyze_task_complexity_with_coherence(prompt: &str, coherence: Option<&CoherenceSignal>) -> f64 {
    let mut score: f64 = 0.0;
    let lower = prompt.to_lowercase();

    // Length heuristic
    if prompt.len() > 2000 { score += 0.2; }
    if prompt.len() > 5000 { score += 0.1; }

    // Keyword indicators
    let complexity_keywords = [
        ("reason", 0.15), ("analyze", 0.15), ("research", 0.15),
        ("architecture", 0.1), ("design", 0.1), ("debug", 0.1),
        ("refactor", 0.1), ("explain", 0.05), ("compare", 0.1),
        ("implement", 0.05), ("algorithm", 0.15), ("optimize", 0.15),
        ("security", 0.1), ("test", 0.05), ("review", 0.05),
        ("plan", 0.1), ("strategy", 0.15), ("system", 0.05),
    ];
    for (keyword, weight) in complexity_keywords {
        if lower.contains(keyword) { score += weight; }
    }

    // Code indicators
    if lower.contains("```") || lower.contains("function") || lower.contains("class ") {
        score += 0.1;
    }

    // Multi-step indicators
    if lower.contains("step") || lower.contains("first") || lower.contains("then") {
        score += 0.05;
    }

    // ── Coherence-aware adjustment (Improvement #6) ──────────────────────
    // When the user is in a stressed/frustrated state (negative valence or high drift),
    // boost complexity to route to more capable models for better support.
    if let Some(cs) = coherence {
        // High drift → boost complexity (more capable model for stability)
        if cs.drift_score > 0.3 {
            score += cs.drift_score * 0.15;
        }
        // Negative valence → the user may be struggling, use stronger model
        if cs.current_valence < -0.1 {
            score += (-cs.current_valence) * 0.1;
        }
    }

    score.min(1.0)
}

/// Route a request to the best model.
pub fn route_request(
    prompt: &str,
    privacy_level: Option<&str>,
    available_models: &[String],
    prefer_local: bool,
) -> ModelRoute {
    route_request_with_coherence(prompt, privacy_level, available_models, prefer_local, None)
}

/// Route a request to the best model, optionally factoring in coherence state.
pub fn route_request_with_coherence(
    prompt: &str,
    privacy_level: Option<&str>,
    available_models: &[String],
    prefer_local: bool,
    coherence: Option<&CoherenceSignal>,
) -> ModelRoute {
    let complexity = analyze_task_complexity_with_coherence(prompt, coherence);
    let is_strict_local = privacy_level == Some("strict_local") || prefer_local;

    // Filter candidates: always exclude embedding-only models from chat routing,
    // and apply the local/cloud preference on top.
    let candidates: Vec<&String> = if is_strict_local {
        available_models
            .iter()
            .filter(|m| !is_cloud_model(m) && !is_embedding_model(m))
            .collect()
    } else {
        available_models
            .iter()
            .filter(|m| !is_embedding_model(m))
            .collect()
    };

    if candidates.is_empty() {
        // Fall back to the configured inference model rather than a hard-coded default.
        let fallback = std::env::var("AMPARO_INFERENCE_MODEL")
            .unwrap_or_else(|_| "llama3.1:8b".to_string());
        return ModelRoute {
            model: fallback,
            is_cloud: false,
            reasoning: "No chat-capable models available, using configured fallback".to_string(),
            complexity_score: complexity,
        };
    }

    // Always prefer the configured inference model when available.
    let preferred = std::env::var("AMPARO_INFERENCE_MODEL")
        .unwrap_or_else(|_| "llama3.1:8b".to_string());
    if let Some(pref) = candidates.iter().find(|m| m.as_str() == preferred.as_str()) {
        return ModelRoute {
            model: pref.to_string(),
            is_cloud: false,
            reasoning: format!("Preferred model '{}' selected", preferred),
            complexity_score: complexity,
        };
    }

    // For complex tasks, prefer cloud models when allowed
    if complexity > 0.6 && !is_strict_local {
        if let Some(cloud) = candidates.iter().find(|m| is_cloud_model(m)) {
            return ModelRoute {
                model: cloud.to_string(),
                is_cloud: true,
                reasoning: format!("Complex task (score {:.1}) routed to cloud model", complexity),
                complexity_score: complexity,
            };
        }
    }

    // For medium tasks, prefer larger local models
    if complexity > 0.3 {
        for model in &candidates {
            let lower = model.to_lowercase();
            if lower.contains("70b") || lower.contains("72b") || lower.contains("large") {
                return ModelRoute {
                    model: model.to_string(),
                    is_cloud: is_cloud_model(model),
                    reasoning: format!("Medium task (score {:.1}) routed to larger model", complexity),
                    complexity_score: complexity,
                };
            }
        }
    }

    // Default: use first available model
    ModelRoute {
        model: candidates[0].to_string(),
        is_cloud: is_cloud_model(candidates[0]),
        reasoning: format!("Simple task (score {:.1}) using default model", complexity),
        complexity_score: complexity,
    }
}

// ──────────────────────────────────── Prompt Cache ────────────────────────────
/// Caches system prompts and repeated context to reduce token usage.
/// Uses content-addressable hashing for cache keys.

use std::collections::HashMap as StdHashMap;

/// Content-addressable cache for system prompts and repeated context, keyed by
/// content hash to reduce token usage.
pub struct PromptCache {
    entries: StdHashMap<String, CachedPrompt>,
    max_entries: usize,
}

#[derive(Clone)]
struct CachedPrompt {
    #[allow(dead_code)]
    content: String,
    token_estimate: usize,
    hit_count: u64,
}

impl PromptCache {
    /// Create an empty cache that evicts the least-hit entry once `max_entries`
    /// is reached.
    pub fn new(max_entries: usize) -> Self {
        Self {
            entries: StdHashMap::new(),
            max_entries,
        }
    }

    /// Get a cached prompt by content hash. Returns None on miss.
    pub fn get(&mut self, content: &str) -> Option<usize> {
        let key = Self::hash(content);
        if let Some(entry) = self.entries.get_mut(&key) {
            entry.hit_count += 1;
            Some(entry.token_estimate)
        } else {
            None
        }
    }

    /// Insert or update a cached prompt.
    pub fn insert(&mut self, content: &str, token_estimate: usize) {
        let key = Self::hash(content);
        if self.entries.len() >= self.max_entries {
            if let Some(evict_key) = self.entries.iter()
                .min_by_key(|(_, v)| v.hit_count)
                .map(|(k, _)| k.clone())
            {
                self.entries.remove(&evict_key);
            }
        }
        self.entries.insert(key, CachedPrompt {
            content: content.to_string(),
            token_estimate,
            hit_count: 1,
        });
    }

    /// Clear the cache.
    pub fn clear(&mut self) {
        self.entries.clear();
    }

    fn hash(content: &str) -> String {
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};
        let mut hasher = DefaultHasher::new();
        content.hash(&mut hasher);
        format!("{:x}", hasher.finish())
    }
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// Serializes the tests that read or remove `AMPARO_INFERENCE_*` env
    /// vars — the suite runs in parallel and they would race otherwise.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Run `f` with exactly the given `AMPARO_INFERENCE_*` vars installed,
    /// restoring the prior environment afterwards. Callers hold `ENV_LOCK`.
    fn with_inference_env(vars: &[(&str, &str)], f: impl FnOnce()) {
        const MANAGED: &[&str] = &[
            "AMPARO_INFERENCE_URL",
            "AMPARO_INFERENCE_MODEL",
            "AMPARO_INFERENCE_KEY",
            "AMPARO_INFERENCE_PROVIDER",
            "AMPARO_INFERENCE_TIMEOUT_SECS",
            "AMPARO_INFERENCE_MAX_TOKENS",
            "AMPARO_INFERENCE_MODEL_ALLOWLIST",
        ];
        let saved: Vec<(String, Option<String>)> = MANAGED
            .iter()
            .map(|k| (k.to_string(), std::env::var(k).ok()))
            .collect();
        for (k, _) in &saved {
            std::env::remove_var(k);
        }
        for (k, v) in vars {
            std::env::set_var(k, v);
        }
        f();
        for (k, v) in saved {
            match v {
                Some(v) => std::env::set_var(&k, v),
                None => std::env::remove_var(&k),
            }
        }
    }

    #[test]
    fn is_cloud_model_identifies_cloud_tags() {
        assert!(is_cloud_model("nemotron-3-super:cloud"));
        assert!(is_cloud_model("qwen3-coder:cloud-latest"));
        assert!(!is_cloud_model("llama3:8b"));
        assert!(!is_cloud_model("qwen3:4b"));
    }

    #[test]
    fn recommend_model_strict_local_excludes_cloud() {
        let available = vec![
            "llama3:8b".to_string(),
            "nemotron:cloud".to_string(),
            "qwen3:4b".to_string(),
        ];
        let (model, is_cloud) = recommend_model("chat", Some("strict_local"), &available);
        assert!(!is_cloud);
        assert!(!model.contains(":cloud"));
    }

    #[test]
    fn recommend_model_prefers_cloud_for_complex_tasks() {
        let available = vec![
            "llama3:8b".to_string(),
            "nemotron:cloud".to_string(),
        ];
        let (model, is_cloud) = recommend_model("research analysis", None, &available);
        assert!(is_cloud);
        assert_eq!(model, "nemotron:cloud");
    }

    #[test]
    fn recommend_model_fallback_on_empty() {
        let available: Vec<String> = vec![];
        let (model, _) = recommend_model("chat", None, &available);
        assert_eq!(model, "llama3");
    }

    #[test]
    fn inference_config_from_env_fails_closed_without_url() {
        // There is no silent localhost default: from_env must error when the
        // required vars are unset.
        let _guard = ENV_LOCK.lock().unwrap();
        with_inference_env(&[], || {
            let err = InferenceConfig::from_env().unwrap_err();
            assert!(matches!(err, InferenceError::Config(_)));
            assert!(err.to_string().contains("AMPARO_INFERENCE_URL"));
        });
    }

    #[test]
    fn from_env_resolves_catalog_id_kimi_to_openai_wire() {
        let _guard = ENV_LOCK.lock().unwrap();
        with_inference_env(
            &[
                ("AMPARO_INFERENCE_URL", "https://api.moonshot.cn/v1"),
                ("AMPARO_INFERENCE_MODEL", "kimi-k3"),
                ("AMPARO_INFERENCE_PROVIDER", "kimi"),
            ],
            || {
                let cfg = InferenceConfig::from_env().unwrap();
                // The alias resolves to the catalog id and the OpenAI wire.
                assert_eq!(cfg.provider, ProviderKind::OpenAI);
                assert_eq!(cfg.provider_id.as_deref(), Some("moonshot"));
                let provider = cfg.build().unwrap();
                assert_eq!(provider.default_model(), "kimi-k3");
            },
        );
    }

    #[test]
    fn from_env_unknown_provider_lists_the_catalog() {
        let _guard = ENV_LOCK.lock().unwrap();
        with_inference_env(
            &[
                ("AMPARO_INFERENCE_URL", "https://example.com/v1"),
                ("AMPARO_INFERENCE_MODEL", "some-model"),
                ("AMPARO_INFERENCE_PROVIDER", "hal9000"),
            ],
            || {
                let err = InferenceConfig::from_env().unwrap_err();
                // The exact error the Kimi drill hit now names what IS accepted.
                assert!(err.to_string().contains("known providers"), "{err}");
                assert!(err.to_string().contains("moonshot"), "{err}");
            },
        );
    }

    #[test]
    fn inference_config_build_rejects_model_outside_allowlist() {
        let cfg = InferenceConfig {
            base_url: "http://localhost:11434/v1".into(),
            api_key: String::new(),
            model: "qwen2.5:14b".into(),
            provider: ProviderKind::OpenAI,
            provider_id: None,
            timeout_secs: DEFAULT_TIMEOUT_SECS,
            max_tokens_limit: None,
            allowlist: Some(vec!["claude-sonnet-5-20250929".into()]),
            cloud: CloudConfig::default(),
        };
        let err = match cfg.build() {
            Err(e) => e,
            Ok(_) => panic!("build() must reject models outside the allowlist"),
        };
        assert!(matches!(err, InferenceError::Config(_)));
        assert!(err.to_string().contains("ALLOWLIST"));
    }

    #[test]
    fn inference_config_build_anthropic_with_allowlisted_model() {
        let cfg = InferenceConfig {
            base_url: "https://api.anthropic.com".into(),
            api_key: "sk-test".into(),
            model: "claude-sonnet-5-20250929".into(),
            provider: ProviderKind::Anthropic,
            provider_id: None,
            timeout_secs: 60,
            max_tokens_limit: Some(4096),
            allowlist: Some(vec!["claude-sonnet-5-20250929".into()]),
            cloud: CloudConfig::default(),
        };
        let provider = cfg.build().unwrap();
        assert_eq!(provider.default_model(), "claude-sonnet-5-20250929");
    }

    #[test]
    fn clamp_max_tokens_applies_limit_and_default() {
        assert_eq!(clamp_max_tokens(None, None), DEFAULT_MAX_TOKENS);
        assert_eq!(clamp_max_tokens(Some(50), None), 50);
        assert_eq!(clamp_max_tokens(Some(99999), Some(4096)), 4096);
        assert_eq!(clamp_max_tokens(Some(100), Some(4096)), 100);
    }

    #[test]
    fn chat_message_tool_calls_serialize_for_openai_wire() {
        let msg = ChatMessage::assistant_tool_calls(vec![AssistantToolCall {
            id: "call_1".into(),
            call_type: "function".into(),
            function: FunctionCall { name: "read".into(), arguments: "{}".into() },
        }]);
        let v = serde_json::to_value(&msg).unwrap();
        let tc = &v["tool_calls"][0];
        assert_eq!(tc["id"], "call_1");
        assert_eq!(tc["type"], "function");
        assert_eq!(tc["function"]["name"], "read");

        // tool role carries the call id it answers
        let tool = ChatMessage::tool("call_1", "ok");
        let v = serde_json::to_value(&tool).unwrap();
        assert_eq!(v["tool_call_id"], "call_1");
        assert!(v.get("tool_calls").is_none());

        // plain messages omit both enriched fields on the wire
        let plain = ChatMessage::user("hi");
        let v = serde_json::to_value(&plain).unwrap();
        assert!(v.get("tool_calls").is_none());
        assert!(v.get("tool_call_id").is_none());
    }

    #[test]
    fn inference_handle_build_fails_closed() {
        // Handle constructors inherit config validation.
        let cfg = InferenceConfig {
            base_url: "http://localhost:11434/v1".into(),
            api_key: String::new(),
            model: "qwen2.5:14b".into(),
            provider: ProviderKind::OpenAI,
            provider_id: None,
            timeout_secs: DEFAULT_TIMEOUT_SECS,
            max_tokens_limit: None,
            allowlist: Some(vec!["other-model".into()]),
            cloud: CloudConfig::default(),
        };
        assert!(InferenceHandle::new(cfg).is_err());
    }

    #[test]
    fn inference_request_defaults() {
        let req = InferenceRequest::default();
        assert!(req.prompt.is_empty());
        assert!(req.max_tokens.is_none());
        assert!(req.privacy_level.is_none());
        assert!(req.json_schema.is_none());
        assert!(req.thinking.is_none());
    }

    #[test]
    fn chat_request_structured_output_fields() {
        let req = ChatRequest {
            messages: vec![ChatMessage {
                role: "user".into(),
                content: "hello".into(),
                tool_calls: None,
                tool_call_id: None,
                reasoning_content: None,
            }],
            tools: None,
            max_tokens: Some(100),
            temperature: Some(0.5),
            stream: Some(true),
            model: Some("nemotron:cloud".into()),
            privacy_level: Some("cloud_first".into()),
            json_schema: Some(serde_json::json!({"type": "object"})),
            thinking: Some("medium".into()),
            web_search: Some(true),
            challenge_level: None,
        };
        assert_eq!(req.model.as_deref(), Some("nemotron:cloud"));
        assert_eq!(req.thinking.as_deref(), Some("medium"));
        assert_eq!(req.web_search, Some(true));
    }

    #[test]
    fn openai_provider_model_id_parsing() {
        let payload = serde_json::json!({
            "data": [
                {"id": "llama3:8b"},
                {"id": "qwen3:4b"},
            ]
        });
        let ids = OpenAIProvider::parse_model_ids_pub(&payload);
        assert_eq!(ids.len(), 2);
        assert!(ids.contains(&"llama3:8b".to_string()));
        assert!(ids.contains(&"qwen3:4b".to_string()));
    }

    #[test]
    fn openai_provider_model_id_parsing_ollama_format() {
        let payload = serde_json::json!({
            "models": [
                {"name": "gemma3:1b"},
                {"name": "deepseek-r1:14b"},
            ]
        });
        let ids = OpenAIProvider::parse_model_ids_pub(&payload);
        assert_eq!(ids.len(), 2);
    }

    #[test]
    fn openai_provider_model_id_deduplication() {
        let payload = serde_json::json!({
            "data": [
                {"id": "llama3:8b"},
                {"id": "llama3:8b"},
                {"id": "qwen3:4b"},
            ]
        });
        let ids = OpenAIProvider::parse_model_ids_pub(&payload);
        assert_eq!(ids.len(), 2);
    }

    #[test]
    fn temperature_from_challenge_level_1() {
        assert!((temperature_from_challenge(Some(1), 0.7) - 0.3).abs() < f32::EPSILON);
    }

    #[test]
    fn temperature_from_challenge_level_2() {
        assert!((temperature_from_challenge(Some(2), 0.7) - 0.6).abs() < f32::EPSILON);
    }

    #[test]
    fn temperature_from_challenge_level_3() {
        assert!((temperature_from_challenge(Some(3), 0.7) - 0.9).abs() < f32::EPSILON);
    }

    #[test]
    fn temperature_from_challenge_high_levels_cap() {
        assert!((temperature_from_challenge(Some(5), 0.7) - 0.9).abs() < f32::EPSILON);
    }

    #[test]
    fn temperature_from_challenge_none_uses_default() {
        assert!((temperature_from_challenge(None, 0.7) - 0.7).abs() < f32::EPSILON);
        assert!((temperature_from_challenge(None, 0.4) - 0.4).abs() < f32::EPSILON);
    }

    #[test]
    fn analyze_task_complexity_simple() {
        let score = analyze_task_complexity("hello");
        assert!(score < 0.3, "simple greeting should be low complexity");
    }

    #[test]
    fn analyze_task_complexity_code_review() {
        let score = analyze_task_complexity("Please analyze and refactor this architecture to optimize the security algorithm");
        assert!(score > 0.5, "complex code task should be high complexity");
    }

    #[test]
    fn analyze_task_complexity_caps_at_one() {
        let score = analyze_task_complexity("reason analyze research architecture design debug refactor algorithm optimize security test review plan strategy system");
        assert!(score <= 1.0, "complexity should never exceed 1.0");
    }

    #[test]
    fn route_request_prefers_local_for_simple() {
        let models = vec!["llama3:8b".to_string(), "nemotron:cloud".to_string()];
        let route = route_request("hello", None, &models, false);
        assert_eq!(route.model, "llama3:8b");
        assert!(!route.is_cloud);
    }

    #[test]
    fn route_request_prefers_cloud_for_complex() {
        let models = vec!["llama3:8b".to_string(), "nemotron:cloud".to_string()];
        let route = route_request("Please analyze this complex architecture and reason about the algorithm design", None, &models, false);
        assert!(route.is_cloud);
    }

    #[test]
    fn route_request_respects_strict_local() {
        let models = vec!["llama3:8b".to_string(), "nemotron:cloud".to_string()];
        let route = route_request("analyze this complex architecture", Some("strict_local"), &models, false);
        assert!(!route.is_cloud);
        assert_eq!(route.model, "llama3:8b");
    }

    #[test]
    fn route_request_empty_models_falls_back() {
        // `route_request` reads AMPARO_INFERENCE_MODEL as its fallback —
        // this test pins the no-env default, so it shares the env lock and
        // clears the var first.
        let _guard = ENV_LOCK.lock().unwrap();
        std::env::remove_var("AMPARO_INFERENCE_MODEL");
        let models: Vec<String> = vec![];
        let route = route_request("hello", None, &models, false);
        assert_eq!(route.model, "llama3.1:8b");
    }

    #[test]
    fn prompt_cache_miss_returns_none() {
        let mut cache = PromptCache::new(10);
        assert!(cache.get("not cached").is_none());
    }

    #[test]
    fn prompt_cache_hit_returns_value() {
        let mut cache = PromptCache::new(10);
        cache.insert("system prompt", 100);
        assert_eq!(cache.get("system prompt"), Some(100));
    }

    #[test]
    fn prompt_cache_evicts_least_hit() {
        let mut cache = PromptCache::new(2);
        cache.insert("a", 10);
        cache.insert("b", 20);
        cache.get("a");
        cache.insert("c", 30);
        assert!(cache.get("a").is_some());
        assert!(cache.get("b").is_none());
        assert!(cache.get("c").is_some());
    }

    #[test]
    fn prompt_cache_clear() {
        let mut cache = PromptCache::new(10);
        cache.insert("a", 10);
        cache.insert("b", 20);
        cache.clear();
        assert!(cache.get("a").is_none());
        assert!(cache.get("b").is_none());
    }

    // ── FallbackProvider tests ───────────────────────────────────────────

    /// An always-failing provider used to test fallback behavior.
    struct FailingProvider;
    #[async_trait]
    impl InferenceProvider for FailingProvider {
        async fn complete(&self, _request: InferenceRequest) -> Result<InferenceResponse> {
            Err(InferenceError::Provider("simulated failure".into()))
        }
        async fn complete_chat_stream(&self, _request: ChatRequest) -> Result<InferenceStream> {
            Err(InferenceError::Provider("simulated failure".into()))
        }
        async fn embed(&self, _text: &str) -> Result<Vec<f64>> {
            Ok(vec![])
        }
        async fn list_models(&self) -> Result<Vec<String>> {
            Ok(vec![])
        }
        fn default_model(&self) -> String {
            "failing".into()
        }
    }

    /// A simple provider that always returns a fixed response.
    struct OkProvider(String);
    #[async_trait]
    impl InferenceProvider for OkProvider {
        async fn complete(&self, _request: InferenceRequest) -> Result<InferenceResponse> {
            Ok(InferenceResponse { text: self.0.clone(), tokens: 1, finish_reason: "stop".into() })
        }
        async fn complete_chat_stream(&self, _request: ChatRequest) -> Result<InferenceStream> {
            use futures_util::stream;
            let bytes = bytes::Bytes::from("data: [DONE]\n\n");
            Ok(Box::pin(stream::once(async move { Ok(bytes) })))
        }
        async fn embed(&self, _text: &str) -> Result<Vec<f64>> { Ok(vec![]) }
        async fn list_models(&self) -> Result<Vec<String>> { Ok(vec![]) }
        fn default_model(&self) -> String { self.0.clone() }
    }

    #[tokio::test]
    async fn fallback_provider_switches_on_primary_failure() {
        let fallback = FallbackProvider::new(
            Arc::new(FailingProvider),
            Arc::new(OkProvider("fallback-response".into())),
        );
        let resp = fallback.complete(InferenceRequest::default()).await.unwrap();
        assert_eq!(&resp.text, "fallback-response");
    }

    #[tokio::test]
    async fn fallback_provider_uses_primary_when_healthy() {
        let fallback = FallbackProvider::new(
            Arc::new(OkProvider("primary-response".into())),
            Arc::new(OkProvider("fallback-response".into())),
        );
        let resp = fallback.complete(InferenceRequest::default()).await.unwrap();
        assert_eq!(&resp.text, "primary-response");
    }

    #[tokio::test]
    async fn fallback_provider_delegates_embed_to_primary() {
        let fallback = FallbackProvider::new(
            Arc::new(OkProvider("primary".into())),
            Arc::new(OkProvider("fallback".into())),
        );
        let result: Vec<f64> = fallback.embed("text").await.unwrap();
        assert!(result.is_empty());
    }

    #[tokio::test]
    async fn fallback_provider_default_model_uses_primary() {
        let fallback = FallbackProvider::new(
            Arc::new(OkProvider("primary-model".into())),
            Arc::new(OkProvider("fallback-model".into())),
        );
        assert_eq!(&fallback.default_model(), "primary-model");
    }

    // ── Request-shape tests (one-shot HTTP mocks) ──────────────────────

    /// One-shot HTTP mock: accepts a single connection, parses request
    /// line + headers + body, replies with a complete SSE response, and
    /// returns the captured `(method, path, body)` over a channel.
    fn one_shot_http(
        response: &'static str,
    ) -> (String, std::sync::mpsc::Receiver<(String, String, serde_json::Value)>) {
        use std::io::{BufRead, BufReader, Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut line = String::new();
            let _ = reader.read_line(&mut line);
            let mut parts = line.split_whitespace();
            let method = parts.next().unwrap_or("").to_string();
            let path = parts.next().unwrap_or("").to_string();
            let mut content_length = 0usize;
            loop {
                line.clear();
                if reader.read_line(&mut line).unwrap() == 0 {
                    break;
                }
                let trimmed = line.trim_end();
                if trimmed.is_empty() {
                    break;
                }
                if let Some(rest) = trimmed.to_ascii_lowercase().strip_prefix("content-length:") {
                    content_length = rest.trim().parse().unwrap_or(0);
                }
            }
            let mut raw = vec![0u8; content_length];
            if content_length > 0 {
                reader.read_exact(&mut raw).unwrap();
            }
            let body =
                serde_json::from_slice(&raw).unwrap_or(serde_json::Value::Null);
            let _ = tx.send((method, path, body));
            let bytes = response.as_bytes();
            let head = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\
                 Content-Length: {}\r\nConnection: close\r\n\r\n",
                bytes.len()
            );
            let _ = stream.write_all(head.as_bytes());
            let _ = stream.write_all(bytes);
            let _ = stream.flush();
        });
        (format!("http://{addr}"), rx)
    }

    /// A chat request exercising every extension knob at once: thinking
    /// mode, structured output, web search, and an assistant turn that
    /// carries a reasoning trace.
    fn chat_request_with_extras() -> ChatRequest {
        ChatRequest {
            messages: vec![
                ChatMessage::user("hello"),
                ChatMessage {
                    role: "assistant".into(),
                    content: "hi".into(),
                    tool_calls: None,
                    tool_call_id: None,
                    reasoning_content: Some("deep trace".into()),
                },
            ],
            tools: None,
            max_tokens: Some(100),
            temperature: Some(0.7),
            stream: Some(true),
            model: Some("test-model".into()),
            privacy_level: None,
            json_schema: Some(serde_json::json!({"type": "object"})),
            thinking: Some("on".into()),
            web_search: Some(true),
            challenge_level: None,
        }
    }

    async fn drain(stream: InferenceStream) {
        use futures_util::StreamExt;
        let mut stream = stream;
        while let Some(_item) = stream.next().await {}
    }

    fn captured_after_drain(
        rx: &std::sync::mpsc::Receiver<(String, String, serde_json::Value)>,
    ) -> (String, String, serde_json::Value) {
        rx.recv_timeout(std::time::Duration::from_secs(10)).unwrap()
    }

    #[tokio::test]
    async fn compat_stream_body_is_pure_openai_shape() {
        let (url, rx) = one_shot_http("data: [DONE]\n\n");
        let provider = OpenAIProvider::new(
            url,
            "sk-test".into(),
            "kimi-k3".into(),
            10,
            None,
            ProviderKind::OpenAI,
            false,
        );
        let stream = provider
            .complete_chat_stream(chat_request_with_extras())
            .await
            .unwrap();
        drain(stream).await;
        let (method, path, body) = captured_after_drain(&rx);
        assert_eq!(method, "POST");
        assert!(path.ends_with("/chat/completions"), "{path}");
        // Strict endpoints (Moonshot/Kimi, DeepSeek, …) 400 on unknown
        // fields — none of the Ollama/extension knobs leave via this path.
        assert!(body.get("think").is_none());
        assert!(body.get("format").is_none());
        assert!(body.get("web_search").is_none());
        assert!(body.get("response_format").is_some());
        // reasoning=false scrubs the assistant trace before serialization.
        let messages = body["messages"].as_array().unwrap();
        let assistant = messages
            .iter()
            .find(|m| m["role"] == "assistant")
            .unwrap();
        assert!(assistant.get("reasoning_content").is_none());
    }

    #[tokio::test]
    async fn compat_stream_echoes_reasoning_only_when_flagged() {
        let (url, rx) = one_shot_http("data: [DONE]\n\n");
        let provider = OpenAIProvider::new(
            url,
            "sk-test".into(),
            "kimi-k3".into(),
            10,
            None,
            ProviderKind::OpenAI,
            true,
        );
        let stream = provider
            .complete_chat_stream(chat_request_with_extras())
            .await
            .unwrap();
        drain(stream).await;
        let (_, _, body) = captured_after_drain(&rx);
        let messages = body["messages"].as_array().unwrap();
        let assistant = messages
            .iter()
            .find(|m| m["role"] == "assistant")
            .unwrap();
        assert_eq!(assistant["reasoning_content"], "deep trace");
        // The trace is only ever on assistant turns.
        let user = messages.iter().find(|m| m["role"] == "user").unwrap();
        assert!(user.get("reasoning_content").is_none());
    }

    #[tokio::test]
    async fn ollama_native_stream_still_sends_think() {
        let (url, rx) = one_shot_http("data: [DONE]\n\n");
        let provider = OpenAIProvider::new(
            url,
            String::new(),
            "qwen3:4b".into(),
            10,
            None,
            ProviderKind::OllamaNative,
            false,
        );
        let stream = provider
            .complete_chat_stream(chat_request_with_extras())
            .await
            .unwrap();
        drain(stream).await;
        let (_, path, body) = captured_after_drain(&rx);
        assert!(path.ends_with("/api/chat"), "{path}");
        assert_eq!(body["think"], true);
        // The native path uses `format` for structured output — the
        // OpenAI-shaped `response_format` never leaves here.
        assert!(body.get("format").is_some());
        assert!(body.get("response_format").is_none());
    }
}