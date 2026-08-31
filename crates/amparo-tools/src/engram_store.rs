//! Engram memory backend — the recommended store behind the [`Memory`]
//! trait ([`memory`](crate::memory)).
//!
//! A thin `reqwest` client over the engramd REST surface (Engram's
//! `API_SURFACE.md`): `GET /health` for the startup probe,
//! `POST /memories/search` for retrieval, `POST /memories` for storage,
//! with an optional `Bearer` API key for keyed daemons.
//!
//! Engram is recommended, never required: this module only activates when
//! the operator sets `AMPARO_MEMORY_BACKEND=engram`, and every failure
//! degrades — an unreachable daemon at startup falls back to the built-in
//! [`InMemoryStore`] (see [`resolve_memory_backend`]), and a daemon that
//! dies mid-run turns searches into empty results rather than crashing
//! the agent loop.

use super::memory::{InMemoryStore, Memory, MemoryEntry};
use async_trait::async_trait;
use serde::Deserialize;
use std::sync::Arc;
use std::time::Duration;

/// The default engramd base URL, used when `AMPARO_MEMORY_BACKEND=engram`
/// but `AMPARO_ENGRAM_URL` is unset — the documented local daemon address.
const DEFAULT_ENGRAM_URL: &str = "http://127.0.0.1:8787";

/// Per-request timeout: a hung daemon must degrade the search (to an
/// empty result), never hang the agent loop.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// The Engram adapter behind the [`Memory`] trait: `search` and `store`
/// over engramd's REST surface.
pub struct EngramStore {
    client: reqwest::Client,
    base_url: String,
    api_key: Option<String>,
}

impl EngramStore {
    /// Creates an adapter for the engramd REST surface at `base_url`
    /// (a trailing slash is tolerated). `api_key` becomes the `Bearer`
    /// token when the daemon runs in keyed mode (`ENGRAMD_API_KEY`).
    pub fn new(base_url: String, api_key: Option<String>) -> Self {
        let client = reqwest::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .build()
            .expect("reqwest client build");
        Self {
            client,
            base_url: base_url.trim_end_matches('/').to_string(),
            api_key,
        }
    }

    /// Probes the daemon's `GET /health`. [`resolve_memory_backend`]
    /// uses this once at startup to choose between the adapter and the
    /// built-in store.
    pub async fn probe(&self) -> bool {
        self.get("health")
            .send()
            .await
            .is_ok_and(|r| r.status().is_success())
    }

    fn get(&self, path: &str) -> reqwest::RequestBuilder {
        self.authed(self.client.get(format!("{}/{}", self.base_url, path)))
    }

    fn post(&self, path: &str) -> reqwest::RequestBuilder {
        self.authed(self.client.post(format!("{}/{}", self.base_url, path)))
    }

    fn authed(&self, rb: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        match &self.api_key {
            Some(key) => rb.bearer_auth(key),
            None => rb,
        }
    }
}

/// The subset of engramd's memory row the adapter maps onto
/// [`MemoryEntry`]. Both the capture response and each search result row
/// carry these fields; serde ignores everything else (tags, links,
/// valence, …).
#[derive(Debug, Deserialize)]
struct EngramMemory {
    id: String,
    content: String,
    created_at: String,
    #[serde(default)]
    skipped: bool,
    #[serde(default)]
    matched_id: Option<String>,
    #[serde(default)]
    skip_reason: Option<String>,
}

/// The `POST /memories/search` response body — only the ranked `results`
/// matter to the adapter.
#[derive(Debug, Deserialize)]
struct SearchBody {
    results: Vec<EngramMemory>,
}

#[async_trait]
impl Memory for EngramStore {
    async fn search(&self, query: &str, limit: usize) -> Vec<MemoryEntry> {
        // Any failure — the daemon down, a bad response — degrades to no
        // hits. The startup probe already warned when the daemon was down
        // at resolution time; a mid-run outage must not crash the loop.
        let Ok(resp) = self
            .post("memories/search")
            .json(&serde_json::json!({ "query": query, "limit": limit }))
            .send()
            .await
        else {
            return Vec::new();
        };
        if !resp.status().is_success() {
            return Vec::new();
        }
        let Ok(body) = resp.json::<SearchBody>().await else {
            return Vec::new();
        };
        body.results
            .into_iter()
            .map(|m| MemoryEntry {
                id: m.id,
                content: m.content,
                created_at: m.created_at,
            })
            .collect()
    }

    async fn store(&self, content: String) -> Result<String, String> {
        let resp = self
            .post("memories")
            .json(&serde_json::json!({ "content": content }))
            .send()
            .await
            .map_err(|e| format!("engram store failed: {e}"))?;
        if !resp.status().is_success() {
            return Err(format!("engram store failed: HTTP {}", resp.status()));
        }
        let m: EngramMemory = resp
            .json()
            .await
            .map_err(|e| format!("engram store failed: bad response: {e}"))?;
        // engramd's capture can fold the write into an existing row
        // (noise/duplicate filtering): a duplicate is still stored — under
        // the matched row's id — but a filtered capture persisted nothing,
        // and reporting success would be a lie to the agent.
        if m.skipped {
            if let Some(matched) = m.matched_id {
                return Ok(matched);
            }
            return Err(m
                .skip_reason
                .unwrap_or_else(|| "capture filtered".to_string()));
        }
        Ok(m.id)
    }
}

/// Resolve the memory backend a host wires into its registry (M11 W1).
///
/// `AMPARO_MEMORY_BACKEND=engram` selects the Engram adapter: the
/// engramd daemon at `AMPARO_ENGRAM_URL` (default
/// `http://127.0.0.1:8787`), with the optional `AMPARO_ENGRAM_KEY`
/// bearer token. Anything else selects the built-in [`InMemoryStore`].
///
/// When Engram is selected but the daemon is unreachable, one `[memory]`
/// warning is printed and the built-in store stands in — Engram is
/// recommended, never required, and its absence must never strand a run.
pub async fn resolve_memory_backend() -> Arc<dyn Memory> {
    if std::env::var("AMPARO_MEMORY_BACKEND").ok().as_deref() != Some("engram") {
        return Arc::new(InMemoryStore::new());
    }
    let url = std::env::var("AMPARO_ENGRAM_URL").unwrap_or_else(|_| DEFAULT_ENGRAM_URL.to_string());
    let key = std::env::var("AMPARO_ENGRAM_KEY").ok();
    let store = EngramStore::new(url, key);
    if store.probe().await {
        Arc::new(store)
    } else {
        eprintln!("[memory] engram unavailable — using built-in store");
        Arc::new(InMemoryStore::new())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// Spawn a loopback engramd stand-in. `handler` receives each raw
    /// request (head + body) and returns the HTTP status and JSON body
    /// to serve; one request per connection.
    async fn mock_engramd(
        handler: impl Fn(String) -> (u16, String) + Send + Sync + 'static,
    ) -> (String, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let handler = std::sync::Arc::new(handler);
        let handle = tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    break;
                };
                let mut buf = vec![0u8; 8192];
                let _ = sock.read(&mut buf).await;
                let raw = String::from_utf8_lossy(&buf).to_string();
                let (status, json) = handler(raw);
                let reason = if status == 200 { "OK" } else { "Not Found" };
                let resp = format!(
                    "HTTP/1.1 {status} {reason}\r\ncontent-type: application/json\r\n\
                     content-length: {}\r\nconnection: close\r\n\r\n{json}",
                    json.len()
                );
                let _ = sock.write_all(resp.as_bytes()).await;
            }
        });
        (format!("http://{}", addr), handle)
    }

    fn mem(id: &str, content: &str, created: &str) -> String {
        json!({"id": id, "content": content, "created_at": created}).to_string()
    }

    #[tokio::test]
    async fn probe_true_on_healthy_daemon() {
        let (url, handle) = mock_engramd(|raw| {
            let head = raw.to_lowercase();
            assert!(head.contains("get /health "), "unexpected request: {raw}");
            (200, json!({"status":"ok"}).to_string())
        })
        .await;
        assert!(EngramStore::new(url, None).probe().await);
        handle.abort();
    }

    #[tokio::test]
    async fn probe_false_on_unreachable_daemon() {
        // A closed port — the degrade path the resolver keys on.
        assert!(
            !EngramStore::new("http://127.0.0.1:1".to_string(), None)
                .probe()
                .await
        );
    }

    #[tokio::test]
    async fn search_maps_results_to_memory_entries() {
        let (url, handle) = mock_engramd(|raw| {
            let head = raw.to_lowercase();
            assert!(head.contains("post /memories/search "), "unexpected request: {raw}");
            assert!(raw.contains("\"query\":\"hetzner\""), "query not forwarded: {raw}");
            assert!(raw.contains("\"limit\":3"), "limit not forwarded: {raw}");
            let body = json!({
                "results": [json!({"id":"m1","content":"deploys to hetzner","created_at":"2026-08-31T00:00:00Z"})],
                "total": 1,
                "vault_total": 5,
                "search_type": "fts5",
                "took_ms": 1
            });
            (200, body.to_string())
        })
        .await;
        let hits = EngramStore::new(url, None).search("hetzner", 3).await;
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].id, "m1");
        assert_eq!(hits[0].content, "deploys to hetzner");
        handle.abort();
    }

    #[tokio::test]
    async fn search_degrades_to_empty_on_server_error() {
        let (url, handle) = mock_engramd(|_| (500, json!({"error":"db down"}).to_string())).await;
        assert!(EngramStore::new(url, None)
            .search("anything", 5)
            .await
            .is_empty());
        handle.abort();
    }

    #[tokio::test]
    async fn store_returns_the_row_id() {
        let (url, handle) = mock_engramd(|raw| {
            assert!(
                raw.contains("\"content\":\"remember this\""),
                "content not forwarded: {raw}"
            );
            (200, mem("mem-1", "remember this", "2026-08-31T00:00:00Z"))
        })
        .await;
        assert_eq!(
            EngramStore::new(url, None)
                .store("remember this".into())
                .await,
            Ok("mem-1".to_string())
        );
        handle.abort();
    }

    #[tokio::test]
    async fn store_reports_the_matched_row_on_duplicate() {
        let (url, handle) = mock_engramd(|_| {
            let row = json!({
                "id": "mem-9",
                "content": "already known",
                "created_at": "2026-08-31T00:00:00Z",
                "skipped": true,
                "skip_reason": "duplicate of mem-3",
                "matched_id": "mem-3"
            });
            (200, row.to_string())
        })
        .await;
        assert_eq!(
            EngramStore::new(url, None)
                .store("already known".into())
                .await,
            Ok("mem-3".to_string())
        );
        handle.abort();
    }

    #[tokio::test]
    async fn store_errors_on_filtered_capture() {
        let (url, handle) = mock_engramd(|_| {
            let row = json!({
                "id": "mem-9",
                "content": "noise",
                "created_at": "2026-08-31T00:00:00Z",
                "skipped": true,
                "skip_reason": "ignored source: interaction",
                "matched_id": null
            });
            (200, row.to_string())
        })
        .await;
        let err = EngramStore::new(url, None)
            .store("noise".into())
            .await
            .unwrap_err();
        assert!(err.contains("ignored source"), "got: {err}");
        handle.abort();
    }

    #[tokio::test]
    async fn bearer_key_rides_every_request() {
        let (url, handle) = mock_engramd(|raw| {
            if raw.to_lowercase().contains("authorization: bearer sekrit") {
                (200, json!({"status":"ok"}).to_string())
            } else {
                (500, json!({"error":"no token"}).to_string())
            }
        })
        .await;
        assert!(
            EngramStore::new(url, Some("sekrit".to_string()))
                .probe()
                .await
        );
        handle.abort();
    }

    #[tokio::test]
    async fn no_key_sends_no_authorization_header() {
        let (url, handle) = mock_engramd(|raw| {
            if raw.to_lowercase().contains("authorization:") {
                (500, json!({"error":"unexpected auth"}).to_string())
            } else {
                (200, json!({"status":"ok"}).to_string())
            }
        })
        .await;
        assert!(EngramStore::new(url, None).probe().await);
        handle.abort();
    }
}
