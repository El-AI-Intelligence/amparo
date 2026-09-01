//! Org-policy client — the Guardrail Console's deny-only org rule surface
//! (M12 W2).
//!
//! The Guardrail Console exposes per-org **deny-only** policy rules next to
//! the shared engine policy (`GET/POST /api/orgs/{id}/policies`,
//! `PUT/DELETE …/{rule_id}`, `PUT /api/orgs/{id}/settings` for the
//! audit↔enforce flip, `GET /api/orgs/current` to resolve the org an API
//! key is scoped to). This module is Amparo's client for that surface —
//! the wire the TUI's `/policy` writes ride on.
//!
//! The rules are evaluated by the console proxy on every
//! `/api/upstream/check`, never by this client: writing here changes what
//! the console denies, and an org rule can only harden — it denies what
//! the engine allowed, never allows what the engine denied.
//!
//! Guardrail is recommended, never required: every method returns an
//! [`OrgPolicyError`], never a panic, and callers degrade to honest
//! `[policy]` lines. Transport failures (refused, DNS, timeout) map to
//! [`OrgPolicyError::NotConnected`]; server errors carry the console's own
//! message verbatim in [`OrgPolicyError::Http`] — the 402 enforce-tier
//! text included.

use serde::Deserialize;
use std::time::Duration;

use super::engram_store::non_loopback_http;

/// The Guardrail Console root when `AMPARO_CONSOLE_POLICY_URL` is unset —
/// the managed console's address. Moved here from the TUI's link target
/// so every surface names one default.
pub const DEFAULT_CONSOLE_POLICY_URL: &str = "https://guardrail.elai-intelligence.com";

/// Per-request timeout: a hung console must surface as `not connected`,
/// never hang the surface that asked.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// Server bodies ride into errors verbatim — truncate so one misbehaving
/// console cannot flood the terminal.
const MAX_ERROR_BODY: usize = 400;

/// The org a key is scoped to, as `GET /api/orgs/current` reports it.
#[derive(Debug, Clone, Deserialize)]
pub struct OrgInfo {
    /// The org id every policies/settings path addresses. The
    /// `/api/orgs/current` shape calls it `org_id`; the `/api/orgs`
    /// fallback rows (Organization) call it `id` — the alias accepts
    /// both.
    #[serde(alias = "id")]
    pub org_id: String,
    /// The org's display name.
    #[serde(default)]
    pub name: String,
    /// The org's URL slug.
    #[serde(default)]
    pub slug: String,
    /// `audit` (verdicts advisory) or `enforce`.
    #[serde(default)]
    pub enforce_mode: String,
}

/// One deny rule, as the policies API reports it. An `enabled: false`
/// rule is dormant — the console proxy skips it.
#[derive(Debug, Clone, Deserialize)]
pub struct OrgPolicyRule {
    /// The rule id the toggle/remove paths address.
    pub id: String,
    /// The org the rule belongs to (echoed by the API).
    #[serde(default)]
    pub org_id: String,
    /// The wire check's key the rule denies (`shell`, `network_http`, …).
    pub tool_name: String,
    /// Why the rule exists — shown by `/policy list`.
    #[serde(default)]
    pub reason: String,
    /// Whether the proxy enforces the rule right now.
    pub enabled: bool,
    /// The user id that created the rule.
    #[serde(default)]
    pub created_by: String,
    /// Creation timestamp (console `datetime('now')` format).
    #[serde(default)]
    pub created_at: String,
}

/// Everything that can go wrong talking to the console — callers match
/// each variant into one honest `[policy]` line.
#[derive(Debug)]
pub enum OrgPolicyError {
    /// No credentials configured (the hint says what to set) or the
    /// console is unreachable (the hint carries the transport error).
    NotConnected(String),
    /// The console answered with an error status; `message` is the
    /// console's own body, verbatim — the 402 enforce-tier text rides
    /// here unchanged.
    Http {
        /// The HTTP status the console answered with.
        status: u16,
        /// The console's own error body, verbatim (truncated at 400 chars).
        message: String,
    },
    /// The console answered success but not in the documented shape.
    BadResponse(String),
}

/// The Guardrail Console org-policy client.
///
/// Built from the same credentials the policy checks use —
/// `AMPARO_POLICY_KEY` (a `gk_` org key) and `AMPARO_CONSOLE_POLICY_URL`
/// (the console root, default [`DEFAULT_CONSOLE_POLICY_URL`]) — so the
/// TUI writes rules with the key whose org's checks they govern.
#[derive(Debug)]
pub struct OrgPolicyClient {
    client: reqwest::Client,
    base_url: String,
    api_key: String,
}

impl OrgPolicyClient {
    /// Builds the client from the environment. Missing or empty
    /// `AMPARO_POLICY_KEY` → [`OrgPolicyError::NotConnected`] with the
    /// pairing hint; the console URL falls back to
    /// [`DEFAULT_CONSOLE_POLICY_URL`].
    pub fn from_env() -> Result<Self, OrgPolicyError> {
        let Some(key) = std::env::var("AMPARO_POLICY_KEY")
            .ok()
            .filter(|k| !k.is_empty())
        else {
            return Err(OrgPolicyError::NotConnected(
                "set AMPARO_POLICY_KEY (a gk_ org key) — `guardrail link` pairs this machine"
                    .into(),
            ));
        };
        let url = std::env::var("AMPARO_CONSOLE_POLICY_URL")
            .ok()
            .filter(|u| !u.is_empty())
            .unwrap_or_else(|| DEFAULT_CONSOLE_POLICY_URL.to_string());
        Ok(Self::new(url, key))
    }

    /// Creates a client for the console at `base_url` (the console ROOT —
    /// this client appends its own `/api/…` paths; a pasted trailing
    /// `/api` or `/api/upstream` suffix is stripped back). `api_key`
    /// rides every request as `Authorization: Bearer …`.
    ///
    /// A non-loopback plaintext `http` URL draws one stderr warning: the
    /// key would travel in the clear.
    pub fn new(base_url: String, api_key: String) -> Self {
        let base_url = normalize_base(&base_url);
        if non_loopback_http(&base_url) {
            eprintln!(
                "[policy] console url {base_url} is non-loopback http — \
the gk_ key travels in the clear (use https or a loopback address)"
            );
        }
        let client = reqwest::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .build()
            .expect("reqwest client build");
        Self {
            client,
            base_url,
            api_key,
        }
    }

    /// `GET /api/orgs/current` — the org the key is scoped to.
    ///
    /// Consoles without that endpoint (it shipped with the org-rules
    /// surface) fall back to `GET /api/orgs`, which works only when the
    /// key's account spans exactly one org; zero or several orgs error
    /// with guidance.
    pub async fn current_org(&self) -> Result<OrgInfo, OrgPolicyError> {
        match self.json(reqwest::Method::GET, "api/orgs/current", None).await {
            Ok(body) => serde_json::from_value(body).map_err(|e| {
                OrgPolicyError::BadResponse(format!(
                    "unexpected /api/orgs/current response: {e}"
                ))
            }),
            Err(OrgPolicyError::Http { status: 404, .. }) => {
                let body = self.json(reqwest::Method::GET, "api/orgs", None).await?;
                let orgs: Vec<OrgInfo> = serde_json::from_value(body).map_err(|e| {
                    OrgPolicyError::BadResponse(format!("unexpected /api/orgs response: {e}"))
                })?;
                match orgs.len() {
                    1 => Ok(orgs.into_iter().next().expect("length is one")),
                    0 => Err(OrgPolicyError::Http {
                        status: 404,
                        message: "the console has no /api/orgs/current and the key's \
account belongs to no orgs"
                            .into(),
                    }),
                    n => Err(OrgPolicyError::Http {
                        status: 404,
                        message: format!(
                            "the console has no /api/orgs/current and the key's \
account spans {n} orgs — upgrade the console"
                        ),
                    }),
                }
            }
            Err(other) => Err(other),
        }
    }

    /// The org's deny rules, newest first.
    pub async fn list_rules(&self) -> Result<Vec<OrgPolicyRule>, OrgPolicyError> {
        let org = self.current_org().await?;
        self.list_rules_for(&org).await
    }

    /// The rules of an already-resolved org — the write methods share it
    /// so one command costs one org resolution, not two.
    async fn list_rules_for(&self, org: &OrgInfo) -> Result<Vec<OrgPolicyRule>, OrgPolicyError> {
        let body = self
            .json(
                reqwest::Method::GET,
                &format!("api/orgs/{}/policies", org.org_id),
                None,
            )
            .await?;
        let rules: RulesBody = serde_json::from_value(body).map_err(|e| {
            OrgPolicyError::BadResponse(format!("unexpected /orgs/{{id}}/policies response: {e}"))
        })?;
        Ok(rules.rules)
    }

    /// Adds a deny rule for `tool_name` (the wire check's key). The
    /// console enforces one rule per tool per org — a duplicate is a
    /// 409 with toggle guidance, surfaced as [`OrgPolicyError::Http`].
    pub async fn deny(&self, tool_name: &str, reason: &str) -> Result<OrgPolicyRule, OrgPolicyError> {
        let tool_name = tool_name.trim();
        if tool_name.is_empty() {
            return Err(OrgPolicyError::BadResponse(
                "a deny rule needs a tool name (the wire check's key, e.g. shell)".into(),
            ));
        }
        let org = self.current_org().await?;
        let body = self
            .json(
                reqwest::Method::POST,
                &format!("api/orgs/{}/policies", org.org_id),
                Some(serde_json::json!({
                    "tool_name": tool_name,
                    "reason": reason.trim(),
                })),
            )
            .await?;
        let resp: RuleBody = serde_json::from_value(body).map_err(|e| {
            OrgPolicyError::BadResponse(format!("unexpected rule response: {e}"))
        })?;
        Ok(resp.rule)
    }

    /// Flips the rule for `tool_name`: enabled → disabled, disabled →
    /// enabled. No rule → [`OrgPolicyError::Http`] 404 with creation
    /// guidance.
    pub async fn toggle(&self, tool_name: &str) -> Result<OrgPolicyRule, OrgPolicyError> {
        let org = self.current_org().await?;
        let rule = find_rule(self.list_rules_for(&org).await?, tool_name)?;
        let body = self
            .json(
                reqwest::Method::PUT,
                &format!("api/orgs/{}/policies/{}", org.org_id, rule.id),
                Some(serde_json::json!({ "enabled": !rule.enabled })),
            )
            .await?;
        let resp: RuleBody = serde_json::from_value(body).map_err(|e| {
            OrgPolicyError::BadResponse(format!("unexpected rule response: {e}"))
        })?;
        Ok(resp.rule)
    }

    /// Removes the rule for `tool_name`. No rule → [`OrgPolicyError::Http`]
    /// 404.
    pub async fn remove(&self, tool_name: &str) -> Result<(), OrgPolicyError> {
        let org = self.current_org().await?;
        let rule = find_rule(self.list_rules_for(&org).await?, tool_name)?;
        self.json(
            reqwest::Method::DELETE,
            &format!("api/orgs/{}/policies/{}", org.org_id, rule.id),
            None,
        )
        .await?;
        Ok(())
    }

    /// Flips the org's mode: `"enforce"` or `"audit"`. A console tier
    /// gate answers 402 — the message rides through verbatim.
    pub async fn set_mode(&self, mode: &str) -> Result<(), OrgPolicyError> {
        if !matches!(mode, "enforce" | "audit") {
            return Err(OrgPolicyError::BadResponse(format!(
                "org mode must be 'enforce' or 'audit', got '{mode}'"
            )));
        }
        let org = self.current_org().await?;
        self.json(
            reqwest::Method::PUT,
            &format!("api/orgs/{}/settings", org.org_id),
            Some(serde_json::json!({ "enforce_mode": mode })),
        )
        .await?;
        Ok(())
    }

    /// One request, mapped to the three-way error contract: transport
    /// failures (refused, DNS, timeout) → [`OrgPolicyError::NotConnected`];
    /// error statuses carry the console's own body verbatim (truncated);
    /// success bodies parse into JSON or → [`OrgPolicyError::BadResponse`].
    async fn json(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Option<serde_json::Value>,
    ) -> Result<serde_json::Value, OrgPolicyError> {
        let url = format!("{}/{}", self.base_url, path);
        let rb = self
            .client
            .request(method, url)
            .bearer_auth(&self.api_key);
        let rb = match body {
            Some(value) => rb.json(&value),
            None => rb,
        };
        let resp = rb.send().await.map_err(|e| {
            OrgPolicyError::NotConnected(format!("console unreachable: {e}"))
        })?;
        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            let message = text.chars().take(MAX_ERROR_BODY).collect();
            return Err(OrgPolicyError::Http {
                status: status.as_u16(),
                message,
            });
        }
        resp.json().await.map_err(|e| {
            OrgPolicyError::BadResponse(format!("unexpected response from /{path}: {e}"))
        })
    }
}

/// The `{"rules": […]}` envelope the policies list uses.
#[derive(Debug, Deserialize)]
struct RulesBody {
    rules: Vec<OrgPolicyRule>,
}

/// The `{"rule": …}` envelope create/toggle answer with.
#[derive(Debug, Deserialize)]
struct RuleBody {
    rule: OrgPolicyRule,
}

/// The org's rule for `tool`, or the not-found error the surface turns
/// into "create one first" guidance.
fn find_rule(rules: Vec<OrgPolicyRule>, tool: &str) -> Result<OrgPolicyRule, OrgPolicyError> {
    match rules.into_iter().find(|r| r.tool_name == tool) {
        Some(rule) => Ok(rule),
        None => Err(OrgPolicyError::Http {
            status: 404,
            message: format!("no rule for '{tool}' — /policy deny {tool} creates one"),
        }),
    }
}

/// The console ROOT from a user-supplied URL: trailing slashes dropped,
/// and a pasted `/api` or `/api/upstream` suffix (the engine check URL
/// shape) stripped back to the root — this client appends its own
/// `/api/…` paths.
fn normalize_base(url: &str) -> String {
    let trimmed = url.trim_end_matches('/');
    for suffix in ["/api/upstream", "/api"] {
        if trimmed.to_ascii_lowercase().ends_with(suffix) {
            return trimmed[..trimmed.len() - suffix.len()]
                .trim_end_matches('/')
                .to_string();
        }
    }
    trimmed.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// Spawn a loopback console stand-in. `handler` receives each
    /// request's index (first request = 0) and raw text (head + body) and
    /// returns the status and body to serve; one request per connection,
    /// served in order.
    async fn mock_console(
        handler: impl Fn(usize, String) -> (u16, String) + Send + Sync + 'static,
    ) -> (String, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let counter = std::sync::atomic::AtomicUsize::new(0);
        let handler = std::sync::Arc::new(handler);
        let handle = tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    break;
                };
                let mut buf = vec![0u8; 8192];
                let _ = sock.read(&mut buf).await;
                let raw = String::from_utf8_lossy(&buf).to_string();
                let idx = counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                let (status, body) = handler(idx, raw);
                let reason = match status {
                    200 | 201 => "OK",
                    400 => "Bad Request",
                    401 => "Unauthorized",
                    402 => "Payment Required",
                    404 => "Not Found",
                    409 => "Conflict",
                    500 => "Server Error",
                    _ => "Error",
                };
                let resp = format!(
                    "HTTP/1.1 {status} {reason}\r\ncontent-type: application/json\r\n\
                     content-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = sock.write_all(resp.as_bytes()).await;
            }
        });
        (format!("http://{}", addr), handle)
    }

    fn client(url: &str) -> OrgPolicyClient {
        OrgPolicyClient::new(url.to_string(), "gk_test".to_string())
    }

    fn org_body() -> String {
        json!({
            "org_id": "org-a",
            "name": "Org A",
            "slug": "org-a",
            "enforce_mode": "audit",
        })
        .to_string()
    }

    fn rule_body(enabled: bool) -> String {
        json!({
            "id": "rule-1",
            "org_id": "org-a",
            "tool_name": "shell",
            "reason": "untrusted inputs",
            "enabled": enabled,
            "created_by": "u1",
            "created_at": "2026-09-01T00:00:00Z",
        })
        .to_string()
    }

    fn rules_body() -> String {
        json!({ "rules": [json!({
            "id": "rule-1",
            "org_id": "org-a",
            "tool_name": "shell",
            "reason": "untrusted inputs",
            "enabled": true,
            "created_by": "u1",
            "created_at": "2026-09-01T00:00:00Z",
        })] })
        .to_string()
    }

    // -----------------------------------------------------------------------
    // Construction from the environment
    // -----------------------------------------------------------------------

    #[test]
    fn from_env_requires_the_key_and_defaults_the_url() {
        // No key → NotConnected with the pairing hint (empty string counts
        // as unset).
        std::env::remove_var("AMPARO_POLICY_KEY");
        let err = OrgPolicyClient::from_env().unwrap_err();
        match &err {
            OrgPolicyError::NotConnected(hint) => {
                assert!(hint.contains("AMPARO_POLICY_KEY"), "{hint}");
                assert!(hint.contains("guardrail link"), "{hint}");
            }
            other => panic!("expected NotConnected, got {other:?}"),
        }
        std::env::set_var("AMPARO_POLICY_KEY", "");
        assert!(matches!(
            OrgPolicyClient::from_env(),
            Err(OrgPolicyError::NotConnected(_))
        ));
        // Key set, URL unset → the client builds against the default root.
        std::env::set_var("AMPARO_POLICY_KEY", "gk_env");
        std::env::remove_var("AMPARO_CONSOLE_POLICY_URL");
        let client = OrgPolicyClient::from_env().unwrap();
        assert_eq!(client.base_url, DEFAULT_CONSOLE_POLICY_URL);
        assert_eq!(client.api_key, "gk_env");
        // And an explicit URL wins over the default.
        std::env::set_var("AMPARO_CONSOLE_POLICY_URL", "http://127.0.0.1:1");
        let client = OrgPolicyClient::from_env().unwrap();
        assert_eq!(client.base_url, "http://127.0.0.1:1");
    }

    // -----------------------------------------------------------------------
    // Org resolution
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn current_org_parses_the_keyed_shape() {
        let (url, handle) = mock_console(|idx, raw| {
            assert_eq!(idx, 0);
            let head = raw.to_lowercase();
            assert!(head.contains("get /api/orgs/current "), "unexpected request: {raw}");
            assert!(
                head.contains("authorization: bearer gk_test"),
                "key not sent: {raw}"
            );
            (200, org_body())
        })
        .await;
        let org = client(&url).current_org().await.unwrap();
        assert_eq!(org.org_id, "org-a");
        assert_eq!(org.name, "Org A");
        assert_eq!(org.slug, "org-a");
        assert_eq!(org.enforce_mode, "audit");
        handle.abort();
    }

    #[tokio::test]
    async fn current_org_falls_back_to_a_single_org_list() {
        let (url, handle) = mock_console(|idx, raw| {
            let head = raw.to_lowercase();
            match idx {
                0 => {
                    assert!(head.contains("get /api/orgs/current "), "unexpected request: {raw}");
                    (404, json!({"error": "session caller"}).to_string())
                }
                1 => {
                    assert!(head.contains("get /api/orgs "), "unexpected request: {raw}");
                    (200, json!([{
                        "id": "org-a",
                        "name": "Org A",
                        "slug": "org-a",
                        "enforce_mode": "audit",
                    }])
                    .to_string())
                }
                _ => panic!("unexpected request #{idx}: {raw}"),
            }
        })
        .await;
        let org = client(&url).current_org().await.unwrap();
        assert_eq!(org.org_id, "org-a");
        handle.abort();
    }

    #[tokio::test]
    async fn current_org_ambiguous_fallback_errors_with_guidance() {
        let (url, handle) = mock_console(|idx, raw| {
            match idx {
                0 => (404, json!({"error": "session caller"}).to_string()),
                1 => {
                    let two = json!([{"id": "a"}, {"id": "b"}]).to_string();
                    assert!(raw.contains("/api/orgs "), "unexpected request: {raw}");
                    (200, two)
                }
                _ => panic!("unexpected request #{idx}: {raw}"),
            }
        })
        .await;
        match client(&url).current_org().await.unwrap_err() {
            OrgPolicyError::Http { status, message } => {
                assert_eq!(status, 404);
                assert!(message.contains("spans 2 orgs"), "{message}");
            }
            other => panic!("expected Http 404, got {other:?}"),
        }
        handle.abort();
    }

    // -----------------------------------------------------------------------
    // Rules: list / deny / toggle / remove
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn list_rules_resolves_the_org_then_lists() {
        let (url, handle) = mock_console(|idx, raw| {
            let head = raw.to_lowercase();
            match idx {
                0 => (200, org_body()),
                1 => {
                    assert!(
                        head.contains("get /api/orgs/org-a/policies "),
                        "unexpected request: {raw}"
                    );
                    (200, rules_body())
                }
                _ => panic!("unexpected request #{idx}: {raw}"),
            }
        })
        .await;
        let rules = client(&url).list_rules().await.unwrap();
        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0].id, "rule-1");
        assert_eq!(rules[0].tool_name, "shell");
        assert_eq!(rules[0].reason, "untrusted inputs");
        assert!(rules[0].enabled);
        handle.abort();
    }

    #[tokio::test]
    async fn deny_posts_the_rule_and_returns_it() {
        let (url, handle) = mock_console(|idx, raw| {
            let head = raw.to_lowercase();
            match idx {
                0 => (200, org_body()),
                1 => {
                    assert!(
                        head.contains("post /api/orgs/org-a/policies "),
                        "unexpected request: {raw}"
                    );
                    assert!(
                        raw.contains("\"tool_name\":\"shell\""),
                        "tool_name not forwarded: {raw}"
                    );
                    assert!(
                        raw.contains("\"reason\":\"untrusted inputs\""),
                        "reason not forwarded: {raw}"
                    );
                    (201, json!({ "rule": {
                        "id": "rule-1",
                        "org_id": "org-a",
                        "tool_name": "shell",
                        "reason": "untrusted inputs",
                        "enabled": true,
                        "created_by": "u1",
                        "created_at": "2026-09-01T00:00:00Z",
                    }})
                    .to_string())
                }
                _ => panic!("unexpected request #{idx}: {raw}"),
            }
        })
        .await;
        let rule = client(&url)
            .deny("  shell ", "  untrusted inputs ")
            .await
            .unwrap();
        assert_eq!(rule.id, "rule-1");
        assert!(rule.enabled);
        handle.abort();
    }

    #[tokio::test]
    async fn deny_conflict_surfaces_the_console_message() {
        let (url, handle) = mock_console(|idx, _raw| match idx {
            0 => (200, org_body()),
            1 => (409, "A rule for 'shell' already exists; toggle it instead".to_string()),
            _ => panic!("unexpected request #{idx}"),
        })
        .await;
        match client(&url).deny("shell", "").await.unwrap_err() {
            OrgPolicyError::Http { status, message } => {
                assert_eq!(status, 409);
                assert!(
                    message.contains("already exists; toggle it instead"),
                    "{message}"
                );
            }
            other => panic!("expected Http 409, got {other:?}"),
        }
        handle.abort();
    }

    #[tokio::test]
    async fn deny_rejects_an_empty_tool_name_locally() {
        // No requests may fire for a caller bug — a bare-URL client would
        // only confuse the assertion if one did.
        let client = client("http://127.0.0.1:1");
        assert!(matches!(
            client.deny("   ", "why").await,
            Err(OrgPolicyError::BadResponse(_))
        ));
    }

    #[tokio::test]
    async fn toggle_flips_the_rule_enabled_flag() {
        let (url, handle) = mock_console(|idx, raw| {
            let head = raw.to_lowercase();
            match idx {
                0 => (200, org_body()),
                1 => (200, rules_body()),
                2 => {
                    assert!(
                        head.contains("put /api/orgs/org-a/policies/rule-1 "),
                        "unexpected request: {raw}"
                    );
                    assert!(raw.contains("\"enabled\":false"), "flip not sent: {raw}");
                    (200, json!({ "rule": {
                        "id": "rule-1",
                        "org_id": "org-a",
                        "tool_name": "shell",
                        "reason": "untrusted inputs",
                        "enabled": false,
                        "created_by": "u1",
                        "created_at": "2026-09-01T00:00:00Z",
                    }})
                    .to_string())
                }
                _ => panic!("unexpected request #{idx}: {raw}"),
            }
        })
        .await;
        let rule = client(&url).toggle("shell").await.unwrap();
        assert!(!rule.enabled);
        handle.abort();
    }

    #[tokio::test]
    async fn toggle_unknown_tool_is_404_guidance() {
        let (url, handle) = mock_console(|idx, _raw| match idx {
            0 => (200, org_body()),
            1 => (200, json!({ "rules": [] }).to_string()),
            _ => panic!("unexpected request #{idx}"),
        })
        .await;
        match client(&url).toggle("shell").await.unwrap_err() {
            OrgPolicyError::Http { status, message } => {
                assert_eq!(status, 404);
                assert!(message.contains("no rule for 'shell'"), "{message}");
                assert!(message.contains("/policy deny shell creates one"), "{message}");
            }
            other => panic!("expected Http 404, got {other:?}"),
        }
        handle.abort();
    }

    #[tokio::test]
    async fn remove_deletes_by_resolved_rule() {
        let (url, handle) = mock_console(|idx, raw| {
            let head = raw.to_lowercase();
            match idx {
                0 => (200, org_body()),
                1 => (200, rules_body()),
                2 => {
                    assert!(
                        head.contains("delete /api/orgs/org-a/policies/rule-1 "),
                        "unexpected request: {raw}"
                    );
                    (200, json!({"status": "removed"}).to_string())
                }
                _ => panic!("unexpected request #{idx}: {raw}"),
            }
        })
        .await;
        client(&url).remove("shell").await.unwrap();
        handle.abort();
    }

    // -----------------------------------------------------------------------
    // Mode flip
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn set_mode_puts_the_settings_flip() {
        let (url, handle) = mock_console(|idx, raw| {
            let head = raw.to_lowercase();
            match idx {
                0 => (200, org_body()),
                1 => {
                    assert!(
                        head.contains("put /api/orgs/org-a/settings "),
                        "unexpected request: {raw}"
                    );
                    assert!(
                        raw.contains("\"enforce_mode\":\"enforce\""),
                        "mode not sent: {raw}"
                    );
                    (200, json!({"status": "ok"}).to_string())
                }
                _ => panic!("unexpected request #{idx}: {raw}"),
            }
        })
        .await;
        client(&url).set_mode("enforce").await.unwrap();
        handle.abort();
    }

    #[tokio::test]
    async fn set_mode_402_tier_text_rides_through_verbatim() {
        let (url, handle) = mock_console(|idx, _raw| match idx {
            0 => (200, org_body()),
            1 => (
                402,
                "Enforce mode requires the Pro plan or above. Your current plan is free."
                    .to_string(),
            ),
            _ => panic!("unexpected request #{idx}"),
        })
        .await;
        match client(&url).set_mode("enforce").await.unwrap_err() {
            OrgPolicyError::Http { status, message } => {
                assert_eq!(status, 402);
                assert_eq!(
                    message,
                    "Enforce mode requires the Pro plan or above. Your current plan is free."
                );
            }
            other => panic!("expected Http 402, got {other:?}"),
        }
        handle.abort();
    }

    #[tokio::test]
    async fn set_mode_rejects_unknown_modes_locally() {
        let client = client("http://127.0.0.1:1");
        match client.set_mode("block").await.unwrap_err() {
            OrgPolicyError::BadResponse(message) => {
                assert!(message.contains("'enforce' or 'audit'"), "{message}");
            }
            other => panic!("expected BadResponse, got {other:?}"),
        }
    }

    // -----------------------------------------------------------------------
    // Degradation matrix
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn unreachable_console_is_not_connected() {
        let client = client("http://127.0.0.1:1");
        match client.list_rules().await.unwrap_err() {
            OrgPolicyError::NotConnected(hint) => {
                assert!(hint.contains("console unreachable"), "{hint}");
            }
            other => panic!("expected NotConnected, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn server_error_maps_to_http_500() {
        let (url, handle) = mock_console(|idx, _raw| match idx {
            0 => (500, json!({"error": "db down"}).to_string()),
            _ => panic!("unexpected request #{idx}"),
        })
        .await;
        match client(&url).list_rules().await.unwrap_err() {
            OrgPolicyError::Http { status, .. } => assert_eq!(status, 500),
            other => panic!("expected Http 500, got {other:?}"),
        }
        handle.abort();
    }

    #[tokio::test]
    async fn unauthorized_maps_to_http_401_with_the_body() {
        let (url, handle) = mock_console(|idx, _raw| match idx {
            0 => (401, "unauthorized".to_string()),
            _ => panic!("unexpected request #{idx}"),
        })
        .await;
        match client(&url).list_rules().await.unwrap_err() {
            OrgPolicyError::Http { status, message } => {
                assert_eq!(status, 401);
                assert_eq!(message, "unauthorized");
            }
            other => panic!("expected Http 401, got {other:?}"),
        }
        handle.abort();
    }

    #[tokio::test]
    async fn malformed_success_body_is_bad_response() {
        let (url, handle) = mock_console(|idx, _raw| match idx {
            0 => (200, "not json".to_string()),
            _ => panic!("unexpected request #{idx}"),
        })
        .await;
        assert!(matches!(
            client(&url).current_org().await,
            Err(OrgPolicyError::BadResponse(_))
        ));
        handle.abort();
    }

    // -----------------------------------------------------------------------
    // Base-URL normalization
    // -----------------------------------------------------------------------

    #[test]
    fn normalize_base_strips_api_suffixes_and_slashes() {
        assert_eq!(normalize_base("https://console.example.com"), "https://console.example.com");
        assert_eq!(normalize_base("https://console.example.com/"), "https://console.example.com");
        assert_eq!(normalize_base("https://console.example.com/api"), "https://console.example.com");
        assert_eq!(normalize_base("https://console.example.com/api/"), "https://console.example.com");
        assert_eq!(normalize_base("https://console.example.com/api/upstream"), "https://console.example.com");
        assert_eq!(normalize_base("https://console.example.com/API/upstream/"), "https://console.example.com");
        // A non-API path is left alone — it is not our suffix.
        assert_eq!(normalize_base("https://console.example.com/app"), "https://console.example.com/app");
    }
}
