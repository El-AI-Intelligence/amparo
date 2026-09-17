//! The open policy-check wire protocol client — Amparo's reference
//! implementation of `POST /check {tool_name, target} → {verdict, reason,
//! enforced}`.
//!
//! This module is the Rust reference client for the published wire spec
//! (`ellm-guardrail/docs/WIRE-SPEC.md`). [Guardrail](https://elai-intelligence.com)
//! is the commercial implementation; any conforming engine works.

use crate::{PolicyDecision, PolicyEngine, PolicyVerdict};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::time::Duration;

/// Default timeout for a policy check. The check sits in the hot path of tool
/// execution — a hung engine must not hang the agent.
pub const DEFAULT_CHECK_TIMEOUT_SECS: u64 = 60;

/// The `fired` marker that identifies an audit-only (`enforced: false`)
/// verdict: the engine's real verdict was a prediction, never a block.
/// [`crate::AuditNoticeEngine`] keys its one-time stderr notice off this
/// marker — producer and consumer share one constant.
pub const AUDIT_ONLY_MARKER: &str = "audit-only";

/// The request body per the wire spec.
///
/// `agent_id` is the kernel-minted opaque principal (`agent:<fnv1a-hex>`)
/// this check is attributed to. It is an additive field (WIRE-SPEC §11):
/// callers never synthesize it client-side — it is derived from the kernel's
/// own hash function — and unregistered callers send `"anonymous"` or omit it.
#[derive(Debug, Clone, Serialize)]
pub struct CheckRequest {
    /// The registry tool name (`run_command`, `write_file`, …).
    pub tool_name: String,
    /// The primary argument — the shell command for `run_command`, the path for file tools.
    pub target: String,
    /// Supplementary key/value pairs for the check.
    #[serde(default)]
    pub params: BTreeMap<String, String>,
    /// Caller identifier sent with the check; defaults to `"amparo"`.
    #[serde(default = "default_source")]
    pub source: String,
    /// Marks the check as blocking; this client always sends `false` so audit mode never blocks.
    #[serde(default)]
    pub blocking: bool,
    /// Optional session id attached to every check to correlate engine-side audit rows.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    /// Optional kernel-minted agent principal (`agent:<fnv1a-hex>`) naming the
    /// agent this check is attributed to (WIRE-SPEC §11).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent_id: Option<String>,
}

fn default_source() -> String {
    "amparo".to_string()
}

/// The response body per the wire spec (the fields the caller contract acts on).
#[derive(Debug, Clone, Deserialize)]
pub struct CheckResponse {
    /// Wire verdict string: `allow`, `deny`, or `escalate`.
    pub verdict: Option<String>,
    /// Human-readable explanation for the verdict.
    pub reason: Option<String>,
    /// Console surface only. Absent (engine-direct) means `enforced: true`.
    #[serde(default)]
    pub enforced: Option<bool>,
    /// The engine's real verdict when the response is audit-only (`enforced: false`).
    #[serde(default)]
    pub engine_verdict: Option<String>,
    /// Hard-block flag; when `true` it bypasses audit mode by design.
    #[serde(default)]
    pub limit_reached: Option<bool>,
    /// Engine-side error text, used as the reason when `reason` is absent.
    #[serde(default)]
    pub error: Option<String>,
}

impl CheckResponse {
    /// Engine-direct responses lack `enforced`; treat them as authoritative.
    fn is_enforced(&self) -> bool {
        self.enforced.unwrap_or(true)
    }
}

/// A remote policy engine speaking the open wire protocol.
///
/// Failure posture (fail-safe): any transport error, timeout, or malformed
/// response maps to `Escalate` — an engine failure is **never** an allow.
pub struct WirePolicyEngine {
    client: reqwest::Client,
    base_url: String,
    api_key: Option<String>,
    session_id: Option<String>,
    agent_id: Option<String>,
    timeout: Duration,
}

impl WirePolicyEngine {
    /// `base_url` is the engine's `/check` root (e.g. `https://engine.example`).
    /// `api_key` becomes `Authorization: Bearer <key>`.
    pub fn new(base_url: impl Into<String>, api_key: Option<String>) -> Self {
        Self {
            client: reqwest::Client::builder()
                .connect_timeout(Duration::from_secs(10))
                .build()
                .expect("static reqwest client options are valid"),
            base_url: base_url.into().trim_end_matches('/').to_string(),
            api_key,
            session_id: None,
            agent_id: None,
            timeout: Duration::from_secs(DEFAULT_CHECK_TIMEOUT_SECS),
        }
    }

    /// Attach a session id to every check (correlates engine-side audit rows).
    pub fn with_session_id(mut self, session_id: impl Into<String>) -> Self {
        self.session_id = Some(session_id.into());
        self
    }

    /// Attach a kernel-minted agent principal (`agent:<fnv1a-hex>`) to every
    /// check (WIRE-SPEC §11; attributes engine-side audit rows to the agent).
    pub fn with_agent_id(mut self, agent_id: impl Into<String>) -> Self {
        self.agent_id = Some(agent_id.into());
        self
    }

    /// Override the per-check timeout.
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    async fn check(&self, request: &CheckRequest) -> Result<CheckResponse, String> {
        let mut req = self
            .client
            .post(format!("{}/check", self.base_url))
            .json(request);
        if let Some(key) = &self.api_key {
            req = req.bearer_auth(key);
        }
        let resp = tokio::time::timeout(self.timeout, req.send())
            .await
            .map_err(|_| "policy engine timed out".to_string())?
            .map_err(|e| format!("policy engine unreachable: {e}"))?;

        let status = resp.status();
        // The body read gets its own timeout: `send` resolves when headers
        // arrive, so an engine that stalls mid-body would otherwise hang the
        // loop past the fail-closed budget.
        let body = tokio::time::timeout(self.timeout, resp.bytes())
            .await
            .map_err(|_| "policy engine timed out".to_string())?
            .map_err(|e| format!("bad engine response: {e}"))?;

        if !status.is_success() {
            // The canonical engine 500 carries {"verdict":"escalate"}. Only
            // an escalate may pass through a failure status — any other
            // verdict (an allow on a 500 is an engine bug or a gateway
            // forgery) is a transport-level failure and escalates.
            if let Ok(json) = serde_json::from_slice::<serde_json::Value>(&body) {
                if let Ok(resp) = serde_json::from_value::<CheckResponse>(json) {
                    if resp.verdict.as_deref() == Some("escalate") {
                        return Ok(resp);
                    }
                }
            }
            return Err(format!("policy engine returned HTTP {status}"));
        }

        serde_json::from_slice::<CheckResponse>(&body)
            .map_err(|e| format!("bad engine response: {e}"))
    }

    /// The caller contract, verbatim (wire spec §5).
    fn map_decision(&self, resp: &CheckResponse) -> PolicyDecision {
        let reason = resp
            .reason
            .clone()
            .or_else(|| resp.error.clone())
            .unwrap_or_else(|| "no reason given".to_string());

        // Hard stop — bypasses audit mode by design.
        if resp.limit_reached.unwrap_or(false) {
            return PolicyDecision::deny(format!("plan limit reached: {reason}"));
        }

        let enforced = resp.is_enforced();
        match resp.verdict.as_deref() {
            Some("allow") => PolicyDecision::allow(),
            Some("deny") if enforced => PolicyDecision::deny(reason),
            Some("escalate") if enforced => PolicyDecision::escalate(reason),
            // Audit mode: the verdict is a prediction — never block on it.
            // Proceed, but carry the engine's real verdict in `fired` so the
            // audit trail is honest.
            Some(v @ ("deny" | "escalate")) => {
                let real = resp.engine_verdict.clone().unwrap_or_else(|| v.to_string());
                PolicyDecision {
                    verdict: PolicyVerdict::Allow,
                    fired: vec![format!(
                        "{AUDIT_ONLY_MARKER} (enforced:false): engine verdict {real} — {reason}; proceeding unenforced"
                    )],
                }
            }
            Some(other) => {
                PolicyDecision::escalate(format!("unknown engine verdict {other:?}: {reason}"))
            }
            None => PolicyDecision::escalate(format!("engine response missing verdict: {reason}")),
        }
    }
}

#[async_trait]
impl PolicyEngine for WirePolicyEngine {
    async fn judge_tool(
        &self,
        tool_name: &str,
        target: &str,
        params: &[(&str, &str)],
    ) -> PolicyDecision {
        let request = CheckRequest {
            tool_name: tool_name.to_string(),
            target: target.to_string(),
            params: params
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            source: default_source(),
            blocking: false,
            session_id: self.session_id.clone(),
            agent_id: self.agent_id.clone(),
        };
        match self.check(&request).await {
            Ok(resp) => self.map_decision(&resp),
            Err(e) => PolicyDecision::escalate(format!("policy engine failure: {e}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// Tiny single-request HTTP responder for canned engine responses.
    async fn mock_engine(body: &str) -> (String, tokio::task::JoinHandle<()>) {
        let (url, handle, _) = mock_engine_recording(body).await;
        (url, handle)
    }

    /// Like [`mock_engine`], but also records every raw request body into
    /// the returned mutex so tests can assert what the wire client sent
    /// (M9 W4: session-id serialization).
    async fn mock_engine_recording(
        body: &str,
    ) -> (
        String,
        tokio::task::JoinHandle<()>,
        std::sync::Arc<tokio::sync::Mutex<Vec<String>>>,
    ) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let body = body.to_string();
        let bodies = std::sync::Arc::new(tokio::sync::Mutex::new(Vec::new()));
        let recorded = std::sync::Arc::clone(&bodies);
        let handle = tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = vec![0u8; 8192];
            let _ = sock.read(&mut buf).await.unwrap();
            recorded
                .lock()
                .await
                .push(String::from_utf8_lossy(&buf).to_string());
            let resp = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            sock.write_all(resp.as_bytes()).await.unwrap();
        });
        (format!("http://{}", addr), handle, bodies)
    }

    fn engine(url: &str) -> WirePolicyEngine {
        WirePolicyEngine::new(url, None).with_timeout(Duration::from_secs(5))
    }

    #[tokio::test]
    async fn deny_enforced_blocks_with_reason() {
        let (url, server) = mock_engine(
            &json!({"verdict":"deny","reason":"rm with destructive flags","enforced":true})
                .to_string(),
        )
        .await;
        let d = engine(&url)
            .judge_tool("run_command", "rm -rf /", &[])
            .await;
        assert_eq!(d.verdict, PolicyVerdict::Deny);
        assert!(d.fired[0].contains("rm"));
        server.await.unwrap();
    }

    #[tokio::test]
    async fn escalate_enforced_asks_a_human() {
        let (url, server) = mock_engine(
            &json!({"verdict":"escalate","reason":"new tool, no classification","enforced":true})
                .to_string(),
        )
        .await;
        let d = engine(&url).judge_tool("some_future_tool", "x", &[]).await;
        assert_eq!(d.verdict, PolicyVerdict::Escalate);
        server.await.unwrap();
    }

    #[tokio::test]
    async fn audit_only_never_blocks() {
        // enforced:false is audit mode — the verdict is a prediction. The
        // caller must NOT block, but must record the real verdict.
        let (url, server) = mock_engine(
            &json!({"verdict":"deny","reason":"rm with destructive flags","enforced":false,
                    "engine_verdict":"deny"})
            .to_string(),
        )
        .await;
        let d = engine(&url)
            .judge_tool("run_command", "rm -rf /", &[])
            .await;
        assert_eq!(d.verdict, PolicyVerdict::Allow, "audit mode must not block");
        assert!(
            d.fired[0].starts_with(AUDIT_ONLY_MARKER),
            "real verdict must be recorded under the shared marker"
        );
        assert!(d.fired[0].contains("deny"));
        server.await.unwrap();
    }

    #[tokio::test]
    async fn engine_direct_missing_enforced_is_authoritative() {
        // Engine-direct responses lack `enforced` — treat as enforced:true.
        let (url, server) =
            mock_engine(&json!({"verdict":"deny","reason":"blocked"}).to_string()).await;
        let d = engine(&url)
            .judge_tool("run_command", "rm -rf /", &[])
            .await;
        assert_eq!(d.verdict, PolicyVerdict::Deny);
        server.await.unwrap();
    }

    #[tokio::test]
    async fn limit_reached_hard_blocks_even_in_audit_mode() {
        let (url, server) = mock_engine(
            &json!({"verdict":"escalate","reason":"plan limit","enforced":false,
                    "limit_reached":true})
            .to_string(),
        )
        .await;
        let d = engine(&url).judge_tool("run_command", "ls", &[]).await;
        assert_eq!(
            d.verdict,
            PolicyVerdict::Deny,
            "limit_reached bypasses audit mode"
        );
        server.await.unwrap();
    }

    /// A single-request responder that answers with an arbitrary status line
    /// and body (for failure-status tests).
    async fn mock_engine_status(
        status_line: &str,
        body: &str,
    ) -> (String, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let body = body.to_string();
        let status_line = status_line.to_string();
        let handle = tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = vec![0u8; 8192];
            let _ = sock.read(&mut buf).await.unwrap();
            let resp = format!(
                "HTTP/1.1 {status_line}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            sock.write_all(resp.as_bytes()).await.unwrap();
        });
        (format!("http://{}", addr), handle)
    }

    #[tokio::test]
    async fn engine_500_with_escalate_body_is_fail_safe_escalate() {
        // The spec's canonical 500: {"error": "internal check failure",
        // "verdict": "escalate"} — never treat an engine failure as an allow.
        let (url, server) = mock_engine_status(
            "500 Internal Server Error",
            r#"{"error":"internal check failure","verdict":"escalate"}"#,
        )
        .await;
        let d = engine(&url).judge_tool("run_command", "ls", &[]).await;
        assert_eq!(d.verdict, PolicyVerdict::Escalate);
        assert!(d.fired[0].contains("internal check failure"));
        server.await.unwrap();
    }

    #[tokio::test]
    async fn engine_500_with_allow_body_never_allows() {
        // An allow on a failure status is an engine bug or a gateway forgery
        // — the client must escalate, never execute.
        let (url, server) = mock_engine_status(
            "500 Internal Server Error",
            r#"{"verdict":"allow","reason":"looks fine","enforced":true}"#,
        )
        .await;
        let d = engine(&url).judge_tool("run_command", "ls", &[]).await;
        assert_eq!(d.verdict, PolicyVerdict::Escalate);
        assert!(
            d.fired[0].contains("HTTP 500"),
            "a forged allow on a failure status escalates with the status: {}",
            d.fired[0]
        );
        server.await.unwrap();
    }

    #[tokio::test]
    async fn engine_stalled_body_times_out() {
        // Headers arrive but the body never does — the body read must hit
        // its own timeout, not hang the loop past the fail-closed budget.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = vec![0u8; 8192];
            let _ = sock.read(&mut buf).await.unwrap();
            let resp = "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: 100\r\nconnection: keep-alive\r\n\r\n{";
            sock.write_all(resp.as_bytes()).await.unwrap();
            // Hold the socket open without completing the body.
            tokio::time::sleep(Duration::from_secs(2)).await;
        });
        let d = WirePolicyEngine::new(format!("http://{}", addr), None)
            .with_timeout(Duration::from_millis(300))
            .judge_tool("run_command", "ls", &[])
            .await;
        assert_eq!(d.verdict, PolicyVerdict::Escalate);
        assert!(d.fired[0].contains("timed out"));
        server.abort();
    }

    #[tokio::test]
    async fn unreachable_engine_is_fail_safe_escalate() {
        let engine = WirePolicyEngine::new("http://127.0.0.1:1", None) // nothing listens here
            .with_timeout(Duration::from_secs(2));
        let d = engine.judge_tool("run_command", "ls", &[]).await;
        assert_eq!(d.verdict, PolicyVerdict::Escalate);
        assert!(d.fired[0].contains("failure"));
    }

    #[tokio::test]
    async fn allow_passes_with_no_fired_rules() {
        let (url, server) = mock_engine(
            &json!({"verdict":"allow","reason":"read-only","enforced":true}).to_string(),
        )
        .await;
        let d = engine(&url).judge_tool("read_file", "/tmp/x", &[]).await;
        assert_eq!(d.verdict, PolicyVerdict::Allow);
        assert!(d.fired.is_empty());
        server.await.unwrap();
    }

    #[tokio::test]
    async fn session_id_flows_into_the_check_request() {
        let (url, server, bodies) =
            mock_engine_recording(&json!({"verdict":"allow","enforced":true}).to_string()).await;
        let d = WirePolicyEngine::new(url, None)
            .with_session_id("web-1")
            .judge_tool("run_command", "ls", &[])
            .await;
        assert_eq!(d.verdict, PolicyVerdict::Allow);
        let raw = bodies.lock().await;
        assert_eq!(raw.len(), 1);
        assert!(
            raw[0].contains("\"session_id\":\"web-1\""),
            "the check request carries the session id: {}",
            raw[0]
        );
        server.await.unwrap();
    }

    #[tokio::test]
    async fn no_session_id_omits_the_field() {
        let (url, server, bodies) =
            mock_engine_recording(&json!({"verdict":"allow","enforced":true}).to_string()).await;
        let d = WirePolicyEngine::new(url, None)
            .judge_tool("run_command", "ls", &[])
            .await;
        assert_eq!(d.verdict, PolicyVerdict::Allow);
        let raw = bodies.lock().await;
        assert_eq!(raw.len(), 1);
        assert!(
            !raw[0].contains("session_id"),
            "absent session ids are skipped, not sent as null: {}",
            raw[0]
        );
        server.await.unwrap();
    }

    #[tokio::test]
    async fn agent_id_flows_into_the_check_request() {
        let (url, server, bodies) =
            mock_engine_recording(&json!({"verdict":"allow","enforced":true}).to_string()).await;
        let d = WirePolicyEngine::new(url, None)
            .with_agent_id("agent:1a2b3c4d5e6f70")
            .judge_tool("run_command", "ls", &[])
            .await;
        assert_eq!(d.verdict, PolicyVerdict::Allow);
        let raw = bodies.lock().await;
        assert_eq!(raw.len(), 1);
        assert!(
            raw[0].contains("\"agent_id\":\"agent:1a2b3c4d5e6f70\""),
            "the check request carries the agent id: {}",
            raw[0]
        );
        server.await.unwrap();
    }

    #[tokio::test]
    async fn no_agent_id_omits_the_field() {
        let (url, server, bodies) =
            mock_engine_recording(&json!({"verdict":"allow","enforced":true}).to_string()).await;
        let d = WirePolicyEngine::new(url, None)
            .judge_tool("run_command", "ls", &[])
            .await;
        assert_eq!(d.verdict, PolicyVerdict::Allow);
        let raw = bodies.lock().await;
        assert_eq!(raw.len(), 1);
        assert!(
            !raw[0].contains("agent_id"),
            "absent agent ids are skipped, not sent as null: {}",
            raw[0]
        );
        server.await.unwrap();
    }
}
