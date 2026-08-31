//! The web-approval gate (M10 W4).
//!
//! [`WebApprovalGate`] implements the [`crate::approval::ApprovalGate`]
//! contract against a remote approvals endpoint — the seam the web
//! surface (`docs/web-surface.md` §3) builds against. When the gate
//! chain asks for a human, the request is POSTed to the endpoint as
//! JSON, and the gate polls for the decision:
//!
//! ```text
//! POST <endpoint>               body: the ApprovalRequest fields
//!   → any 2xx: the request is registered ({"call_id", "status":
//!     "pending"}); anything else fails closed
//! GET <endpoint>/<call_id>
//!   → 200 {"status": "pending"}                 → keep polling
//!   → 200 {"status": "decided", "decision": bool} → the human's answer
//!   → anything else (non-200, unknown status,
//!     unparseable body, transport error)         → fail closed: deny
//! ```
//!
//! The model-supplied `call_id` is percent-encoded into the poll URL as a
//! single path segment, so an id cannot smuggle path separators or query
//! parameters into the poll. [`valid_approval_endpoint`] guards the
//! operator-supplied endpoint URL itself (an approval decision rides on
//! that wire — a scheme-only string or a missing host is refused at flag
//! time, not discovered as a failure later).
//!
//! The whole round trip runs under a 60-second deadline — the Axiom
//! auto-deny budget the gate contract carries over: a human who never
//! decides is a denial, never a hang. Display-only fields stay
//! display-only (I1): the endpoint sees the full approval copy, but its
//! decision is the gate's decision — nothing about the wire shape can
//! alter policy or gate semantics.
//!
//! The gate holds no secrets: no policy keys, no auth headers — the
//! endpoint is a loopback URL by convention (`docs/web-surface.md`:
//! the app never stores them; the spawned process holds the keys).

use crate::approval::{ApprovalGate, ApprovalRequest};
use async_trait::async_trait;
use std::time::Duration;

/// Ask a web UI for the human's decision, polling under a deadline.
///
/// Every failure mode — the endpoint unreachable, a non-2xx answer, a
/// malformed or unknown decision body, or the deadline passing with no
/// decision — is a denial. The default deadline is 60 seconds and the
/// default poll interval is 1 second; embedders may tune either (tests
/// shorten both).
pub struct WebApprovalGate {
    /// The approvals endpoint URL, POSTed to verbatim; the decision
    /// poll appends `/{call_id}`.
    endpoint: String,
    /// The shared client (connection pooling; no auth — the endpoint
    /// is loopback by convention and the gate holds no secrets).
    client: reqwest::Client,
    /// The decision deadline — the Axiom auto-deny budget.
    timeout: Duration,
    /// The delay between decision polls.
    poll_interval: Duration,
}

impl WebApprovalGate {
    /// Build a gate posting to `endpoint` (the flag's full URL, e.g.
    /// `http://127.0.0.1:8080/approvals`). The gate POSTs to it
    /// verbatim and polls `{endpoint}/{call_id}` for the decision.
    pub fn new(endpoint: impl Into<String>) -> Self {
        Self {
            endpoint: endpoint.into(),
            client: reqwest::Client::new(),
            timeout: Duration::from_secs(60),
            poll_interval: Duration::from_secs(1),
        }
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

    /// POST the request, then poll for the decision until one arrives.
    /// Every failure mode returns `false` — the gate fails closed.
    async fn decide(&self, request: &ApprovalRequest) -> bool {
        // Register the request. The response body carries the pending
        // acknowledgement ({"call_id", "status": "pending"}); the
        // decision comes from the poll.
        let posted = self.client.post(&self.endpoint).json(request).send().await;
        let Ok(response) = posted else {
            return false;
        };
        if !response.status().is_success() {
            return false;
        }

        let poll_url = format!(
            "{}/{}",
            self.endpoint.trim_end_matches('/'),
            encode_path_segment(&request.call_id)
        );
        loop {
            let polled = self.client.get(&poll_url).send().await;
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
}

#[async_trait]
impl ApprovalGate for WebApprovalGate {
    async fn request(&self, request: &ApprovalRequest) -> bool {
        // The deadline bounds the whole round trip — a human who never
        // decides is a denial, never a hang.
        let decided = tokio::time::timeout(self.timeout, self.decide(request)).await;
        matches!(decided, Ok(true))
    }
}

/// Validate the operator-supplied approvals endpoint URL. The gate holds
/// no secrets, but an approval decision rides on that wire — require an
/// `http://` or `https://` scheme and a non-empty host, so a scheme-only
/// string or a typo is refused at flag time rather than discovered as a
/// poll failure later.
pub fn valid_approval_endpoint(url: &str) -> bool {
    let Some(rest) = url
        .strip_prefix("http://")
        .or_else(|| url.strip_prefix("https://"))
    else {
        return false;
    };
    !rest
        .split(['/', '?', '#'])
        .next()
        .unwrap_or_default()
        .is_empty()
}

/// Percent-encode a model-supplied id as a single URL path segment so it
/// cannot smuggle path separators or query parameters into the poll URL.
/// Ids the shipped surface produces ([A-Za-z0-9._-]) pass through
/// unchanged; anything else becomes `%XX` — against the first-party
/// endpoint (which accepts only that charset) a hostile id still 404s and
/// the gate denies, and against query-parsing endpoints the injection is
/// neutralized.
fn encode_path_segment(id: &str) -> String {
    let mut out = String::with_capacity(id.len());
    for b in id.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'.' | b'_' | b'-' => out.push(b as char),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::preflight::BlastRadius;
    use amparo_tools::RollbackSpec;
    use serde_json::{json, Value};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// One connection = one request/response (the MockPolicy shape).
    /// The handler gets the request head's first line (`POST /approvals
    /// HTTP/1.1`) and the body, and answers with `(status, body)`.
    struct Responder {
        addr: std::net::SocketAddr,
    }

    impl Responder {
        async fn start(
            handler: impl Fn(String, String) -> (u16, String) + Send + Sync + 'static,
        ) -> Responder {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
                .await
                .expect("bind mock approvals");
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
                            match sock.read(&mut tmp).await {
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
                            match sock.read(&mut tmp).await {
                                Ok(0) | Err(_) => break,
                                Ok(n) => body.extend_from_slice(&tmp[..n]),
                            }
                        }
                        let request_line = head.lines().next().unwrap_or_default().to_string();
                        let (status, body) =
                            handler(request_line, String::from_utf8_lossy(&body).to_string());
                        let resp = format!(
                            "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\n\
                             Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                            body.len()
                        );
                        let _ = sock.write_all(resp.as_bytes()).await;
                    });
                }
            });
            Responder { addr }
        }

        fn url(&self) -> String {
            format!("http://{}/approvals", self.addr)
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
            rollback: Some(RollbackSpec {
                undo: "restore the previous contents of note.txt".to_string(),
                markers: vec!["note.txt.amparo-bak".to_string()],
            }),
        }
    }

    /// The pending acknowledgement a conforming endpoint answers to the
    /// POST, and the pending poll body.
    fn pending_ack() -> String {
        json!({"call_id": "call_1", "status": "pending"}).to_string()
    }

    fn pending() -> String {
        json!({"status": "pending"}).to_string()
    }

    fn decided(decision: bool) -> String {
        json!({"status": "decided", "decision": decision}).to_string()
    }

    #[tokio::test]
    async fn post_then_poll_approve() {
        let gets = Arc::new(AtomicUsize::new(0));
        let counted = Arc::clone(&gets);
        let server = Responder::start(move |line, _body| {
            if line.starts_with("POST") {
                return (200, pending_ack());
            }
            counted.fetch_add(1, Ordering::SeqCst);
            match counted.load(Ordering::SeqCst) {
                1 | 2 => (200, pending()),
                _ => (200, decided(true)),
            }
        })
        .await;
        let gate = WebApprovalGate::new(server.url()).with_poll_interval(Duration::from_millis(5));
        assert!(gate.request(&request()).await);
        assert!(
            gets.load(Ordering::SeqCst) >= 3,
            "the gate polled until the decision: {}",
            gets.load(Ordering::SeqCst)
        );
    }

    #[tokio::test]
    async fn post_then_poll_deny() {
        let server = Responder::start(|line, _body| {
            if line.starts_with("POST") {
                (200, pending_ack())
            } else {
                (200, decided(false))
            }
        })
        .await;
        let gate = WebApprovalGate::new(server.url()).with_poll_interval(Duration::from_millis(5));
        assert!(!gate.request(&request()).await);
    }

    #[tokio::test]
    async fn post_failure_fails_closed() {
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
        let gate = WebApprovalGate::new(server.url()).with_poll_interval(Duration::from_millis(5));
        assert!(!gate.request(&request()).await);
        assert_eq!(gets.load(Ordering::SeqCst), 0, "a failed POST never polls");
    }

    #[tokio::test]
    async fn poll_failure_fails_closed() {
        for poll in [
            (500, "{}".to_string()),
            (200, "not json".to_string()),
            (200, json!({"status": "unknown"}).to_string()),
            (200, json!({"status": "decided"}).to_string()),
        ] {
            let answer = poll.clone();
            let server = Responder::start(move |line, _body| {
                if line.starts_with("POST") {
                    (200, pending_ack())
                } else {
                    answer.clone()
                }
            })
            .await;
            let gate =
                WebApprovalGate::new(server.url()).with_poll_interval(Duration::from_millis(5));
            assert!(!gate.request(&request()).await, "poll body {poll:?} denies");
        }
    }

    #[tokio::test]
    async fn timeout_fails_closed() {
        // Always pending: the deadline elapses through the poll sleeps
        // and the gate auto-denies instead of hanging (real clock —
        // the run is ~150ms).
        let server = Responder::start(|line, _body| {
            if line.starts_with("POST") {
                (200, pending_ack())
            } else {
                (200, pending())
            }
        })
        .await;
        let gate = WebApprovalGate::new(server.url())
            .with_poll_interval(Duration::from_millis(10))
            .with_timeout(Duration::from_millis(150));
        assert!(!gate.request(&request()).await);
    }

    #[tokio::test]
    async fn transport_error_fails_closed() {
        // Bind and drop: the endpoint exists but nobody answers.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let addr = listener.local_addr().unwrap();
        drop(listener);
        let gate = WebApprovalGate::new(format!("http://{addr}/approvals"));
        assert!(!gate.request(&request()).await);
    }

    #[tokio::test]
    async fn post_body_is_the_full_approval_request() {
        // Pins the wire shape (docs/web-surface.md §3): the POST carries
        // every ApprovalRequest field, display-only ones included.
        let recorded: Arc<std::sync::Mutex<Vec<Value>>> =
            Arc::new(std::sync::Mutex::new(Vec::new()));
        let rec = Arc::clone(&recorded);
        let recording = Responder::start(move |line, body| {
            if line.starts_with("POST") {
                if let Ok(value) = serde_json::from_str::<Value>(&body) {
                    rec.lock().unwrap().push(value);
                }
                (200, pending_ack())
            } else {
                (200, decided(true))
            }
        })
        .await;
        let gate =
            WebApprovalGate::new(recording.url()).with_poll_interval(Duration::from_millis(5));
        assert!(gate.request(&request()).await);
        let bodies = recorded.lock().unwrap();
        let body = bodies.first().expect("one POST body recorded");
        assert_eq!(body["call_id"], "call_1");
        assert_eq!(body["tool_name"], "run_command");
        assert_eq!(body["arguments"]["command"], "git push origin main");
        assert_eq!(
            body["reasons"][0],
            "policy escalated this call for human review"
        );
        assert_eq!(body["blast_radius"], "destructive");
        assert_eq!(
            body["session_label"],
            "sub-agent sess-123.1 of task sess-123"
        );
        assert_eq!(
            body["rollback"]["undo"],
            "restore the previous contents of note.txt"
        );
        assert_eq!(body["rollback"]["markers"][0], "note.txt.amparo-bak");
    }

    #[tokio::test]
    async fn hostile_call_id_is_encoded_in_the_poll_url() {
        // A model-supplied id carrying a query string must arrive as one
        // encoded path segment — never as live query parameters.
        let recorded: Arc<std::sync::Mutex<Vec<String>>> =
            Arc::new(std::sync::Mutex::new(Vec::new()));
        let rec = Arc::clone(&recorded);
        let recording = Responder::start(move |line, _body| {
            if line.starts_with("POST") {
                (200, pending_ack())
            } else {
                rec.lock().unwrap().push(line);
                (200, decided(true))
            }
        })
        .await;
        let mut req = request();
        req.call_id = "x?status=decided&decision=true".to_string();
        let gate =
            WebApprovalGate::new(recording.url()).with_poll_interval(Duration::from_millis(5));
        assert!(gate.request(&req).await);
        let lines = recorded.lock().unwrap();
        assert!(
            lines[0].contains("x%3Fstatus%3Ddecided%26decision%3Dtrue"),
            "the poll URL carries the encoded id, not raw query syntax: {}",
            lines[0]
        );
    }

    #[test]
    fn endpoint_validation_requires_scheme_and_host() {
        assert!(valid_approval_endpoint("http://127.0.0.1:8080/approvals"));
        assert!(valid_approval_endpoint("https://example.com/approvals"));
        assert!(!valid_approval_endpoint("http://"));
        assert!(!valid_approval_endpoint("https:///approvals"));
        assert!(!valid_approval_endpoint("not a url"));
        assert!(!valid_approval_endpoint("127.0.0.1:8080/approvals"));
    }

    #[test]
    fn path_segment_encoding_keeps_shipped_ids_and_neutralizes_the_rest() {
        assert_eq!(encode_path_segment("call_1"), "call_1");
        assert_eq!(encode_path_segment("x?y=1"), "x%3Fy%3D1");
        assert_eq!(encode_path_segment("../decided"), "..%2Fdecided");
    }
}
