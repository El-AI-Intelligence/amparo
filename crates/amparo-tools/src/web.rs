// Originally part of Axiom-OS (MIT, Copyright (c) Pixel Phantom AI).
// Ported to Amparo and relicensed Apache-2.0 — see the repository NOTICE.

//! Web tools — web_search and fetch_url
//!
//! web_search backend priority (first available wins):
//!   1. SearXNG (AMPARO_SEARXNG_URL)        — self-hosted, free, unlimited, real web search
//!   2. Brave Search (AMPARO_BRAVE_API_KEY) — independent index, 2k free/month, real web search
//!   3. DuckDuckGo Instant Answers (default) — knowledge graph, free, no key needed
//!
//! Backends 1 and 2 are full web search engines. Backend 3 is a knowledge-graph
//! API (Wikipedia abstracts + related topics) — works for factual queries but
//! won't return diverse web results. All three are HTTP GET calls to search
//! indexes — no data-center AI inference anywhere in this pipeline.
//! fetch_url fetches a URL and strips HTML to readable markdown-like text.

use super::{ToolCall, ToolExecutor, ToolParam, ToolResult, ToolSchema, ToolTrustTier};
use async_trait::async_trait;
use serde_json::Value;

fn make_result(call: &ToolCall, success: bool, output: Value, summary: String) -> ToolResult {
    ToolResult {
        tool_call_id: call.id.clone(),
        tool_name: call.name.clone(),
        success,
        output,
        display_summary: summary,
        duration_ms: 0,
    }
}

fn arg_str<'a>(call: &'a ToolCall, key: &str) -> Option<&'a str> {
    call.arguments.get(key).and_then(|v| v.as_str())
}

// ─────────────────────────────────────────────── WebSearchTool ────────────

/// Searches the web, trying backends in order (SearXNG, Brave Search,
/// DuckDuckGo Instant Answers) with automatic fallback when one fails.
/// Trusted at `Observational`.
pub struct WebSearchTool;

impl WebSearchTool {
    /// Creates a new [`WebSearchTool`].
    pub fn new() -> Self { Self }

    /// Which backend should we use? Returns (backend, display_name).
    fn select_backend() -> (Backend, &'static str) {
        if std::env::var("AMPARO_SEARXNG_URL").ok().is_some() {
            return (Backend::Searxng, "SearXNG");
        }
        if std::env::var("AMPARO_BRAVE_API_KEY").ok().is_some() {
            return (Backend::Brave, "Brave Search");
        }
        (Backend::DuckduckgoInstant, "DuckDuckGo Instant Answers")
    }

    // ── SearXNG ──────────────────────────────────────────────────────

    async fn searxng_search(query: &str, num: usize) -> anyhow::Result<Vec<SearchResult>> {
        let base_url = std::env::var("AMPARO_SEARXNG_URL")
            .map_err(|_| anyhow::anyhow!("AMPARO_SEARXNG_URL not set"))?;
        let base = base_url.trim_end_matches('/');

        let client = reqwest::Client::builder()
            .user_agent("Amparo/0.1 (companion AI; amparo@localhost)")
            .timeout(std::time::Duration::from_secs(12))
            .build()?;

        // SearXNG JSON API: /search?q=<query>&format=json&categories=general
        let resp: Value = client
            .get(&format!("{}/search", base))
            .query(&[
                ("q", query),
                ("format", "json"),
                ("categories", "general"),
                ("pageno", "1"),
            ])
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;

        let results: Vec<SearchResult> = resp
            .get("results")
            .and_then(|r| r.as_array())
            .cloned()
            .unwrap_or_default()
            .iter()
            .take(num)
            .filter_map(|r| {
                Some(SearchResult {
                    title: r.get("title")?.as_str()?.to_string(),
                    url: r.get("url")?.as_str()?.to_string(),
                    snippet: r.get("content")
                        .or_else(|| r.get("snippet"))
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string(),
                    source: "SearXNG".to_string(),
                })
            })
            .collect();

        Ok(results)
    }

    // ── Brave Search ─────────────────────────────────────────────────

    /// Brave Search API — independent web index, no Google/Bing dependency.
    /// Free tier: 2,000 queries/month. Requires AMPARO_BRAVE_API_KEY.
    async fn brave_search(query: &str, num: usize) -> anyhow::Result<Vec<SearchResult>> {
        let api_key = std::env::var("AMPARO_BRAVE_API_KEY")
            .map_err(|_| anyhow::anyhow!("AMPARO_BRAVE_API_KEY not set"))?;

        let client = reqwest::Client::builder()
            .user_agent("Amparo/0.1 (companion AI; amparo@localhost)")
            .timeout(std::time::Duration::from_secs(12))
            .build()?;

        let resp: Value = client
            .get("https://api.search.brave.com/res/v1/web/search")
            .query(&[
                ("q", query),
                ("count", &num.min(20).to_string()),
            ])
            .header("Accept", "application/json")
            .header("Accept-Encoding", "gzip")
            .header("X-Subscription-Token", &api_key)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;

        let web = resp
            .get("web")
            .and_then(|w| w.get("results"))
            .and_then(|r| r.as_array())
            .cloned()
            .unwrap_or_default();

        let results: Vec<SearchResult> = web
            .iter()
            .filter_map(|r| {
                Some(SearchResult {
                    title: r.get("title")?.as_str()?.to_string(),
                    url: r.get("url")?.as_str()?.to_string(),
                    snippet: r.get("description")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string(),
                    source: "Brave Search".to_string(),
                })
            })
            .collect();

        Ok(results)
    }

    // ── DuckDuckGo Instant Answers (knowledge graph, free, no key) ────

    /// DuckDuckGo Instant Answer API — returns Wikipedia abstracts and
    /// related topics. Good for factual queries (who/what/where) but not a
    /// full web search. Free, no API key, no auth.
    async fn duckduckgo_instant_search(query: &str) -> anyhow::Result<Vec<SearchResult>> {
        let client = reqwest::Client::builder()
            .user_agent("Amparo/0.1 (companion AI; amparo@localhost)")
            .timeout(std::time::Duration::from_secs(12))
            .build()?;

        let url = format!(
            "https://api.duckduckgo.com/?q={}&format=json&no_html=1&skip_disambig=1",
            urlencoding::encode(query)
        );
        let resp: Value = client.get(&url).send().await?.json().await?;

        let mut results = Vec::new();

        // Abstract result (Wikipedia summary)
        if let Some(abstract_text) = resp.get("AbstractText").and_then(|v| v.as_str()) {
            if !abstract_text.is_empty() {
                results.push(SearchResult {
                    title: resp.get("Heading").and_then(|v| v.as_str()).unwrap_or("").to_string(),
                    url: resp.get("AbstractURL").and_then(|v| v.as_str()).unwrap_or("").to_string(),
                    snippet: abstract_text.to_string(),
                    source: "DuckDuckGo Abstract".to_string(),
                });
            }
        }

        // Related topics
        if let Some(topics) = resp.get("RelatedTopics").and_then(|v| v.as_array()) {
            for topic in topics.iter().take(8) {
                if let Some(text) = topic.get("Text").and_then(|v| v.as_str()) {
                    results.push(SearchResult {
                        title: text.chars().take(80).collect(),
                        url: topic.get("FirstURL").and_then(|v| v.as_str()).unwrap_or("").to_string(),
                        snippet: text.to_string(),
                        source: "DuckDuckGo Related".to_string(),
                    });
                }
            }
        }

        Ok(results)
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Backend {
    Searxng,
    Brave,
    DuckduckgoInstant,
}

#[derive(serde::Serialize, serde::Deserialize, Debug, Clone)]
/// One search result returned by a web-search backend.
pub struct SearchResult {
    /// Result title.
    pub title: String,
    /// Result URL.
    pub url: String,
    /// Short excerpt of the result.
    pub snippet: String,
    /// Name of the backend that produced the result.
    pub source: String,
}

#[async_trait]
impl ToolExecutor for WebSearchTool {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "web_search".to_string(),
            description: "Search the web for current information, news, research, or facts. Returns a list of relevant results with snippets. Use when you need information beyond your training data.".to_string(),
            parameters: vec![
                ToolParam {
                    name: "query".to_string(),
                    description: "The search query. Be specific and use relevant keywords.".to_string(),
                    param_type: "string".to_string(),
                    enum_values: None,
                    required: true,
                },
                ToolParam {
                    name: "num_results".to_string(),
                    description: "Number of results to return (1-10, default 5)".to_string(),
                    param_type: "integer".to_string(),
                    enum_values: None,
                    required: false,
                },
            ],
            trust_tier: ToolTrustTier::Observational,
        }
    }

    async fn execute(&self, call: &ToolCall) -> ToolResult {
        let query = match arg_str(call, "query") {
            Some(q) => q.to_string(),
            None => return make_result(call, false, serde_json::json!({"error": "missing query parameter"}), "Search failed: no query".to_string()),
        };
        let num = call.arg_u64("num_results")
            .unwrap_or(5)
            .min(10) as usize;

        let (backend, backend_name) = Self::select_backend();

        let result = match backend {
            Backend::Searxng => Self::searxng_search(&query, num).await,
            Backend::Brave   => Self::brave_search(&query, num).await,
            Backend::DuckduckgoInstant => Self::duckduckgo_instant_search(&query).await,
        };

        match result {
            Ok(mut results) => {
                results.truncate(num);
                let summary = if results.is_empty() {
                    format!("No results found for \"{}\"", query)
                } else {
                    format!("Found {} results for \"{}\" ({})", results.len(), query, backend_name)
                };
                make_result(call, true, serde_json::json!({
                    "query": query,
                    "results": results,
                    "count": results.len(),
                    "backend": backend_name,
                }), summary)
            }
            Err(e) => {
                // Try next backend in the chain on failure
                tracing::warn!("{} failed ({}), trying fallbacks", backend_name, e);

                let fallback_result = match backend {
                    Backend::Searxng => {
                        // Try Brave, then DDG Instant Answers
                        if std::env::var("AMPARO_BRAVE_API_KEY").ok().is_some() {
                            match Self::brave_search(&query, num).await {
                                Ok(r) => Ok((r, "Brave Search (SearXNG fallback)")),
                                Err(be) => {
                                    tracing::warn!("Brave also failed ({}), trying DDG Instant Answers", be);
                                    Self::duckduckgo_instant_search(&query).await
                                        .map(|r| (r, "DuckDuckGo Instant Answers (dual fallback)"))
                                }
                            }
                        } else {
                            Self::duckduckgo_instant_search(&query).await
                                .map(|r| (r, "DuckDuckGo Instant Answers (SearXNG fallback)"))
                        }
                    }
                    Backend::Brave => {
                        Self::duckduckgo_instant_search(&query).await
                            .map(|r| (r, "DuckDuckGo Instant Answers (Brave fallback)"))
                    }
                    Backend::DuckduckgoInstant => {
                        // Last resort — already at the bottom
                        return make_result(call, false, serde_json::json!({
                            "error": e.to_string(),
                            "query": query,
                        }), format!("Search failed: {}", e));
                    }
                };

                match fallback_result {
                    Ok((mut results, fallback_name)) => {
                        results.truncate(num);
                        make_result(call, true, serde_json::json!({
                            "query": query,
                            "results": results,
                            "count": results.len(),
                            "backend": fallback_name,
                        }), format!("Found {} results for \"{}\" ({})", results.len(), query, fallback_name))
                    }
                    Err(final_err) => {
                        make_result(call, false, serde_json::json!({
                            "error": format!("{} (all backends exhausted)", final_err),
                            "query": query,
                        }), format!("Search failed: all backends exhausted"))
                    }
                }
            }
        }
    }
}

// ─────────────────────────────────────────────────── FetchUrlTool ─────────────

/// Fetches a URL and returns its content as readable text, stripping HTML and
/// requiring an `http://` or `https://` scheme. Trusted at `Observational`.
pub struct FetchUrlTool;

impl FetchUrlTool {
    /// Creates a new [`FetchUrlTool`].
    pub fn new() -> Self { Self }

    /// Strip HTML tags and decode entities to readable text.
    fn html_to_text(html: &str) -> String {
        // Remove script/style blocks entirely
        let re_script = regex_lite::Regex::new(r"(?si)<(script|style)[^>]*>.*?</\1>").unwrap();
        let text = re_script.replace_all(html, " ");
        // Remove all remaining HTML tags
        let re_tags = regex_lite::Regex::new(r"<[^>]+>").unwrap();
        let text = re_tags.replace_all(&text, " ");
        // Decode common HTML entities
        let text = text
            .replace("&amp;", "&")
            .replace("&lt;", "<")
            .replace("&gt;", ">")
            .replace("&quot;", "\"")
            .replace("&apos;", "'")
            .replace("&nbsp;", " ")
            .replace("&#39;", "'");
        // Collapse whitespace
        let re_ws = regex_lite::Regex::new(r"\s+").unwrap();
        let text = re_ws.replace_all(&text, " ");
        text.trim().to_string()
    }
}

#[async_trait]
impl ToolExecutor for FetchUrlTool {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "fetch_url".to_string(),
            description: "Fetch the content of a URL and return it as readable text. Good for reading articles, documentation, or web pages referenced in search results.".to_string(),
            parameters: vec![
                ToolParam {
                    name: "url".to_string(),
                    description: "The URL to fetch. Must be http:// or https://".to_string(),
                    param_type: "string".to_string(),
                    enum_values: None,
                    required: true,
                },
                ToolParam {
                    name: "max_chars".to_string(),
                    description: "Maximum characters to return (default 4000, max 12000)".to_string(),
                    param_type: "integer".to_string(),
                    enum_values: None,
                    required: false,
                },
            ],
            trust_tier: ToolTrustTier::Observational,
        }
    }

    async fn execute(&self, call: &ToolCall) -> ToolResult {
        let url = match arg_str(call, "url") {
            Some(u) => u.to_string(),
            None => return make_result(call, false, serde_json::json!({"error": "missing url parameter"}), "Fetch failed: no URL".to_string()),
        };
        if !url.starts_with("http://") && !url.starts_with("https://") {
            return make_result(call, false, serde_json::json!({"error": "URL must start with http:// or https://"}), "Fetch failed: invalid URL scheme".to_string());
        }
        let max_chars = call.arg_u64("max_chars")
            .unwrap_or(4000)
            .min(12000) as usize;

        let client = match reqwest::Client::builder()
            .user_agent("Amparo/0.1 (companion AI; amparo@localhost)")
            .timeout(std::time::Duration::from_secs(20))
            .build()
        {
            Ok(c) => c,
            Err(e) => return make_result(call, false, serde_json::json!({"error": e.to_string()}), "Fetch failed: client build error".to_string()),
        };

        match client.get(&url).send().await {
            Ok(resp) => {
                let status = resp.status().as_u16();
                if !resp.status().is_success() {
                    return make_result(call, false, serde_json::json!({"error": format!("HTTP {}", status), "url": url}), format!("Fetch failed: HTTP {}", status));
                }
                let content_type = resp.headers()
                    .get("content-type")
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("")
                    .to_lowercase();

                match resp.text().await {
                    Ok(body) => {
                        let text = if content_type.contains("html") {
                            Self::html_to_text(&body)
                        } else {
                            body
                        };
                        let truncated = if text.len() > max_chars {
                            format!("{}... [truncated at {} chars]", &text[..max_chars], max_chars)
                        } else {
                            text.clone()
                        };
                        make_result(call, true, serde_json::json!({
                            "url": url,
                            "content": truncated,
                            "char_count": text.len(),
                            "truncated": text.len() > max_chars,
                        }), format!("Fetched {} chars from {}", text.len().min(max_chars), url))
                    }
                    Err(e) => make_result(call, false, serde_json::json!({"error": e.to_string(), "url": url}), format!("Fetch failed reading body: {}", e)),
                }
            }
            Err(e) => make_result(call, false, serde_json::json!({"error": e.to_string(), "url": url}), format!("Fetch failed: {}", e)),
        }
    }
}
