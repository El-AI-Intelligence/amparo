//! The fan-out approval gate (R3).
//!
//! [`FanOutApprovalGate`] composes a *local* human gate (the TUI's
//! single-keypress cards) with the *remote* approvals endpoint — the
//! deployed web app's hub (`docs/web-surface.md` §3). When the gate chain
//! asks for a human:
//!
//! 1. the request is POSTed to the hub (publication),
//! 2. the local gate and the hub's decision poll run side by side,
//! 3. a local decision is POSTed back to the hub, and
//! 4. the gate acts on the **hub's latched state** — never on a local
//!    press directly.
//!
//! The hub is the single point of arbitration: a deny there is permanent,
//! an approve can never override a deny, and every failure mode (publish
//! refused, poll error, deadline) is a denial. The whole round trip runs
//! under the same 60-second deadline as [`crate::web_approval::WebApprovalGate`] — a human who
//! never decides is a denial, never a hang.
//!
//! The optional approval-scoped token rides every POST and poll as a
//! bearer header so a non-loopback hub can tell a real publisher from
//! noise; loopback hubs need none. The gate itself still holds no policy
//! keys — the token is only ever the approval-scoped secret.

use crate::approval::{ApprovalGate, ApprovalRequest};
use crate::web_approval::encode_path_segment;
use async_trait::async_trait;
use std::sync::Arc;
use std::time::Duration;

/// Ask both the local human and the remote hub, deciding on the hub's
/// latched state.
pub struct FanOutApprovalGate {
    /// The surface-local gate (TUI cards) — one half of the fan-out.
    local: Arc<dyn ApprovalGate>,
    /// The hub's approvals endpoint, POSTed to verbatim; the decision
    /// routes append `/{call_id}` and `/{call_id}/decision`.
    endpoint: String,
    /// The shared client (connection pooling).
    client: reqwest::Client,
    /// The approval-scoped bearer token for non-loopback hubs.
    token: Option<String>,
    /// The decision deadline — the Axiom auto-deny budget.
    timeout: Duration,
    /// The delay between decision polls.
    poll_interval: Duration,
}

impl FanOutApprovalGate {
    /// Build a fan-out gate over `local`, publishing to `endpoint` (the
    /// flag's full URL, e.g. `https://hub.example.com/approvals`).
    pub fn new(local: Arc<dyn ApprovalGate>, endpoint: impl Into<String>) -> Self {
        Self {
            local,
            endpoint: endpoint.into(),
            client: reqwest::Client::new(),
            token: None,
            timeout: Duration::from_secs(60),
            poll_interval: Duration::from_secs(1),
        }
    }

    /// Attach the approval-scoped token (read from the environment by the
    /// caller) — sent as a bearer header on every request.
    pub fn with_token(mut self, token: impl Into<String>) -> Self {
        self.token = Some(token.into());
        self
    }

    /// Override the decision deadline (default 60 seconds).
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Override the delay between decision polls (default 1 second).
    pub fn with_poll_interval(mut self, poll_interval: Duration) -> Self {
        self.poll_interval = poll_interval;
        self
    }

    /// The endpoint URL this gate posts to.
    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }

    fn bearer(&self, request: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        match &self.token {
            Some(token) => request.bearer_auth(token),
            None => request,
        }
    }

    /// Publish the request to the hub. A refused publish is a denial —
    /// there is no hub state to arbitrate on.
    async fn publish(&self, request: &ApprovalRequest) -> bool {
        let posted = self
            .bearer(self.client.post(&self.endpoint))
            .json(request)
            .send()
            .await;
        let Ok(response) = posted else {
            return false;
        };
        response.status().is_success()
    }

    /// POST the local human's decision to the hub. A `200` latches it; a
    /// `409` means another surface decided first — the hub's state stands,
    /// and the poll reads it. Anything else fails closed.
    async fn post_decision(&self, request: &ApprovalRequest, decision: bool) -> bool {
        let url = format!(
            "{}/{}/decision",
            self.endpoint.trim_end_matches('/'),
            encode_path_segment(&request.call_id)
        );
        let posted = self
            .bearer(self.client.post(&url))
            .json(&serde_json::json!({ "decision": decision }))
            .send()
            .await;
        let Ok(response) = posted else {
            return false;
        };
        response.status().is_success() || response.status().as_u16() == 409
    }

    /// Poll the hub until it decides. Every failure mode returns `false` —
    /// the gate fails closed.
    async fn poll_until_decided(&self, request: &ApprovalRequest) -> bool {
        let poll_url = format!(
            "{}/{}",
            self.endpoint.trim_end_matches('/'),
            encode_path_segment(&request.call_id)
        );
        loop {
            let polled = self.bearer(self.client.get(&poll_url)).send().await;
            let Ok(response) = polled else {
                return false;
            };
            if response.status().as_u16() != 200 {
                return false;
            }
            let Ok(body) = response.json::<serde_json::Value>().await else {
                return false;
            };
            match body.get("status").and_then(|v| v.as_str()) {
                Some("pending") => {}
                Some("decided") => {
                    return body
                        .get("decision")
                        .and_then(|v| v.as_bool())
                        .unwrap_or(false);
                }
                // An unknown status string is not a decision — deny.
                _ => return false,
            }
            tokio::time::sleep(self.poll_interval).await;
        }
    }

    /// Publish, race local against hub, then take the hub's latched
    /// state. The caller bounds the whole round trip with [`Self::timeout`].
    async fn decide(&self, request: &ApprovalRequest) -> bool {
        if !self.publish(request).await {
            return false;
        }
        tokio::select! {
            // The hub latched first: its state is final — deny dominates
            // there, so no later local press can flip it.
            hub = self.poll_until_decided(request) => return hub,
            local = self.local.request(request) => {
                // The local human decided first: publish their answer,
                // then answer from the hub's latch. A deny the hub already
                // holds (another surface) wins over the local approve.
                if !self.post_decision(request, local).await {
                    return false;
                }
                return self.poll_until_decided(request).await;
            }
        }
    }
}

#[async_trait]
impl ApprovalGate for FanOutApprovalGate {
    async fn request(&self, request: &ApprovalRequest) -> bool {
        // The deadline bounds the whole round trip — a human who never
        // decides is a denial, never a hang.
        let decided = tokio::time::timeout(self.timeout, self.decide(request)).await;
        matches!(decided, Ok(true))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::preflight::BlastRadius;
    use serde_json::{json, Value};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;

    /// One connection = one request/response (the web_approval test
    /// pattern). The handler gets the request head and body and answers
    /// with `(status, body)`.
    struct Responder {
        addr: std::net::SocketAddr,
    }

    impl Responder {
        async fn start(
            handler: impl Fn(String, String) -> (u16, String) + Send + Sync + 'static,
        ) -> Responder {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
                .await
                .expect("bind mock hub");
            let addr = listener.local_addr().unwrap();
            let handler: Arc<dyn Fn(String, String) -> (u16, String) + Send + Sync> =
                Arc::new(handler);
            tokio::spawn(async move {
                loop {
                    let Ok((mut sock, _)) = listener.accept().await else {
                        break;
                    };
                    let handler = Arc::clone(&handler);
                    tokio::spawn(async move {
                        let mut buf = Vec::new();
                        let mut tmp = [0u8; 8192];
                        while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
                            match tokio::io::AsyncReadExt::read(&mut sock, &mut tmp).await {
                                Ok(0) | Err(_) => break,
                                Ok(n) => buf.extend_from_slice(&tmp[..n]),
                            }
                            if buf.len() > 1_000_000 {
                                break;
                            }
                        }
                        let split = buf
                            .windows(4)
                            .position(|w| w == b"\r\n\r\n")
                            .map(|p| p + 4)
                            .unwrap_or(buf.len());
                        let head = String::from_utf8_lossy(&buf[..split]).to_string();
                        let content_length = head
                            .lines()
                            .find_map(|l| {
                                l.trim_start()
                                    .to_ascii_lowercase()
                                    .strip_prefix("content-length:")
                                    .map(|v| v.trim().parse::<usize>().unwrap_or(0))
                            })
                            .unwrap_or(0);
                        let mut body = buf[split..].to_vec();
                        while body.len() < content_length {
                            match tokio::io::AsyncReadExt::read(&mut sock, &mut tmp).await {
                                Ok(0) | Err(_) => break,
                                Ok(n) => body.extend_from_slice(&tmp[..n]),
                            }
                        }
                        // The full head reaches the handler — routing
                        // checks read the first line, the token test reads
                        // the authorization header.
                        let (status, body) =
                            handler(head.clone(), String::from_utf8_lossy(&body).to_string());
                        let resp = format!(
                            "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\n\
                             Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                            body.len()
                        );
                        let _ = tokio::io::AsyncWriteExt::write_all(&mut sock, resp.as_bytes()).await;
                    });
                }
            });
            Responder { addr }
        }

        fn url(&self) -> String {
            format!("http://{}/approvals", self.addr)
        }
    }

    /// A gate that never answers — stands in for a human who walked away.
    struct SilentGate;

    #[async_trait]
    impl ApprovalGate for SilentGate {
        async fn request(&self, _request: &ApprovalRequest) -> bool {
            tokio::time::sleep(Duration::from_secs(3600)).await;
            true
        }
    }

    /// A gate answering a fixed value after a fixed delay.
    struct SlowGate {
        answer: bool,
        delay: Duration,
    }

    #[async_trait]
    impl ApprovalGate for SlowGate {
        async fn request(&self, _request: &ApprovalRequest) -> bool {
            tokio::time::sleep(self.delay).await;
            self.answer
        }
    }

    fn request() -> ApprovalRequest {
        ApprovalRequest {
            call_id: "call_1".to_string(),
            tool_name: "run_command".to_string(),
            arguments: json!({"command": "git push origin main"}),
            reasons: vec!["policy escalated this call for human review".to_string()],
            blast_radius: Some(BlastRadius::Destructive),
            session_label: Some("sub-agent sess-123.1 of task sess-123".to_string()),
            rollback: None,
        }
    }

    fn pending_ack() -> String {
        json!({"call_id": "call_1", "status": "pending"}).to_string()
    }

    fn pending() -> String {
        json!({"status": "pending"}).to_string()
    }

    fn decided(decision: bool) -> String {
        json!({"status": "decided", "decision": decision}).to_string()
    }

    /// The hub approves after two pending polls; the local human never
    /// answers — the hub's state is final on its own.
    #[tokio::test]
    async fn hub_decides_first_approve() {
        let gets = Arc::new(AtomicUsize::new(0));
        let counted = Arc::clone(&gets);
        let server = Responder::start(move |line, _body| {
            if line.starts_with("POST") {
                (200, pending_ack())
            } else {
                counted.fetch_add(1, Ordering::SeqCst);
                match counted.load(Ordering::SeqCst) {
                    1 | 2 => (200, pending()),
                    _ => (200, decided(true)),
                }
            }
        })
        .await;
        let gate = FanOutApprovalGate::new(
            Arc::new(SilentGate),
            server.url(),
        )
        .with_poll_interval(Duration::from_millis(5));
        assert!(gate.request(&request()).await);
        assert!(
            gets.load(Ordering::SeqCst) >= 3,
            "the gate polled until the decision: {}",
            gets.load(Ordering::SeqCst)
        );
    }

    /// A hub deny beats a local approve that races it.
    #[tokio::test]
    async fn hub_deny_wins_over_local_approve() {
        let server = Responder::start(|line, _body| {
            if line.starts_with("POST") {
                (200, pending_ack())
            } else {
                (200, decided(false))
            }
        })
        .await;
        let gate = FanOutApprovalGate::new(
            Arc::new(SlowGate {
                answer: true,
                delay: Duration::from_millis(60),
            }),
            server.url(),
        )
        .with_poll_interval(Duration::from_millis(5));
        assert!(!gate.request(&request()).await);
    }

    /// The local human approves: their decision is POSTed to the hub and
    /// the gate answers from the hub's latched state.
    #[tokio::test]
    async fn local_approve_posts_and_takes_hub_state() {
        // The hub stays pending until the decision POST lands, so the
        // local arm of the select is the one that resolves first.
        let decisions: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let rec = Arc::clone(&decisions);
        let server = Responder::start(move |line, body| {
            if line.starts_with("POST /approvals HTTP") {
                (200, pending_ack())
            } else if line.starts_with("POST") && line.contains("/decision") {
                rec.lock().unwrap().push(body);
                (200, decided(true))
            } else if rec.lock().unwrap().is_empty() {
                (200, pending())
            } else {
                (200, decided(true))
            }
        })
        .await;
        let gate = FanOutApprovalGate::new(
            Arc::new(SlowGate {
                answer: true,
                delay: Duration::from_millis(10),
            }),
            server.url(),
        )
        .with_poll_interval(Duration::from_millis(5));
        assert!(gate.request(&request()).await);
        let bodies = decisions.lock().unwrap();
        assert_eq!(
            bodies.first().map(String::as_str),
            Some("{\"decision\":true}"),
            "the local approve was published to the hub"
        );
    }

    /// The local human denies: the deny is POSTed to the hub (where the
    /// deny-wins latch makes it permanent) and the gate answers from the
    /// hub's state.
    #[tokio::test]
    async fn local_deny_posts_deny_to_hub() {
        let posted: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
        let rec = Arc::clone(&posted);
        let server = Responder::start(move |line, body| {
            if line.starts_with("POST /approvals HTTP") {
                (200, pending_ack())
            } else if line.starts_with("POST") && line.contains("/decision") {
                *rec.lock().unwrap() = Some(body);
                (200, decided(false))
            } else if rec.lock().unwrap().is_none() {
                (200, pending())
            } else {
                (200, decided(false))
            }
        })
        .await;
        let gate = FanOutApprovalGate::new(
            Arc::new(SlowGate {
                answer: false,
                delay: Duration::from_millis(10),
            }),
            server.url(),
        )
        .with_poll_interval(Duration::from_millis(5));
        assert!(!gate.request(&request()).await);
        assert_eq!(
            posted.lock().unwrap().as_deref(),
            Some("{\"decision\":false}"),
            "the local deny was published to the hub"
        );
    }

    /// A 409 on the decision POST (another surface decided first) is not
    /// a failure: the poll reads the hub's latched state.
    #[tokio::test]
    async fn decision_conflict_reads_hub_state() {
        let posts = Arc::new(AtomicUsize::new(0));
        let counted = Arc::clone(&posts);
        let server = Responder::start(move |line, _body| {
            if line.starts_with("POST /approvals HTTP") {
                (200, pending_ack())
            } else if line.starts_with("POST") && line.contains("/decision") {
                counted.fetch_add(1, Ordering::SeqCst);
                (409, json!({"error": "already decided", "decision": false}).to_string())
            } else if counted.load(Ordering::SeqCst) == 0 {
                (200, pending())
            } else {
                (200, decided(false))
            }
        })
        .await;
        let gate = FanOutApprovalGate::new(
            Arc::new(SlowGate {
                answer: true,
                delay: Duration::from_millis(10),
            }),
            server.url(),
        )
        .with_poll_interval(Duration::from_millis(5));
        assert!(!gate.request(&request()).await, "the hub's deny latch wins");
        assert_eq!(
            posts.load(Ordering::SeqCst),
            1,
            "the local press reached the hub's decision route"
        );
    }

    /// A refused publish is a denial — nothing is polled.
    #[tokio::test]
    async fn publish_failure_fails_closed() {
        let gets = Arc::new(AtomicUsize::new(0));
        let counted = Arc::clone(&gets);
        let server = Responder::start(move |line, _body| {
            if line.starts_with("POST") {
                (500, "{}".to_string())
            } else {
                counted.fetch_add(1, Ordering::SeqCst);
                (200, decided(true))
            }
        })
        .await;
        let gate = FanOutApprovalGate::new(Arc::new(SilentGate), server.url());
        assert!(!gate.request(&request()).await);
        assert_eq!(
            gets.load(Ordering::SeqCst),
            0,
            "a failed publish never polls"
        );
    }

    /// A hub that never decides and a human who never answers: the
    /// deadline auto-denies instead of hanging (real clock — ~150ms).
    #[tokio::test]
    async fn timeout_fails_closed() {
        let server = Responder::start(|line, _body| {
            if line.starts_with("POST") {
                (200, pending_ack())
            } else {
                (200, pending())
            }
        })
        .await;
        let gate = FanOutApprovalGate::new(Arc::new(SilentGate), server.url())
            .with_poll_interval(Duration::from_millis(10))
            .with_timeout(Duration::from_millis(150));
        assert!(!gate.request(&request()).await);
    }

    /// The approval-scoped token rides every request as a bearer header —
    /// the publish, the pending polls, the decision POST, and the final
    /// poll that reads the hub's latched state.
    #[tokio::test]
    async fn token_sent_on_publish_decision_and_poll() {
        let heads: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let rec = Arc::clone(&heads);
        let posted: Arc<Mutex<bool>> = Arc::new(Mutex::new(false));
        let flag = Arc::clone(&posted);
        let server = Responder::start(move |line, _body| {
            rec.lock().unwrap().push(line.clone());
            if line.starts_with("POST /approvals HTTP") {
                (200, pending_ack())
            } else if line.starts_with("POST") && line.contains("/decision") {
                *flag.lock().unwrap() = true;
                (200, decided(true))
            } else if !*flag.lock().unwrap() {
                (200, pending())
            } else {
                (200, decided(true))
            }
        })
        .await;
        let gate = FanOutApprovalGate::new(
            Arc::new(SlowGate {
                answer: true,
                delay: Duration::from_millis(10),
            }),
            server.url(),
        )
        .with_token("approval-secret")
        .with_poll_interval(Duration::from_millis(5));
        assert!(gate.request(&request()).await);
        let heads = heads.lock().unwrap();
        // reqwest sends header names lowercase on the wire — match that.
        let has_auth = heads
            .iter()
            .filter(|h| h.to_ascii_lowercase().contains("authorization:"))
            .filter(|h| h.contains("Bearer approval-secret"))
            .count();
        assert_eq!(
            has_auth,
            heads.len(),
            "every request carries the bearer token: {heads:?}"
        );
        assert!(heads.len() >= 4, "publish, poll, decision, final poll: {heads:?}");
    }

    /// The publish carries the full ApprovalRequest wire shape.
    #[tokio::test]
    async fn publish_body_is_the_full_approval_request() {
        let recorded: Arc<Mutex<Vec<Value>>> = Arc::new(Mutex::new(Vec::new()));
        let rec = Arc::clone(&recorded);
        let recording = Responder::start(move |line, body| {
            if line.starts_with("POST /approvals HTTP") {
                if let Ok(value) = serde_json::from_str::<Value>(&body) {
                    rec.lock().unwrap().push(value);
                }
                (200, pending_ack())
            } else if line.starts_with("POST") {
                (200, decided(true))
            } else {
                (200, decided(true))
            }
        })
        .await;
        let gate = FanOutApprovalGate::new(
            Arc::new(SlowGate {
                answer: true,
                delay: Duration::from_millis(10),
            }),
            recording.url(),
        )
        .with_poll_interval(Duration::from_millis(5));
        assert!(gate.request(&request()).await);
        let bodies = recorded.lock().unwrap();
        let body = bodies.first().expect("one POST body recorded");
        assert_eq!(body["call_id"], "call_1");
        assert_eq!(body["tool_name"], "run_command");
        assert_eq!(body["arguments"]["command"], "git push origin main");
        assert_eq!(body["blast_radius"], "destructive");
    }
}
