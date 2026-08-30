//! The `send_notification` tool (M10 W2).
//!
//! The last member of the coordination set: where the blackboard hands
//! state along the delegation chain *inside* the workspace, a
//! notification reaches *outside* it — to a chat, a channel, or a
//! webhook the operator's host configured. Because it leaves the
//! workspace it is [`ToolTrustTier::ExternalEffector`]: every send asks
//! for human approval, and the approval copy carries the call's
//! arguments — so the human approves a *named destination*, not an
//! abstraction.
//!
//! The tool itself knows nothing about wire protocols. It holds a
//! [`NotificationTransport`] — the same seam shape as [`crate::paths::PathPolicy`]
//! — and hosts inject their own:
//!
//! - the chat driver wires its platform transport (the destination is a
//!   chat id the provider can reach),
//! - the CLI wires a configured webhook (`--webhook-url`), or the
//!   stderr default when none is given,
//! - the MCP server keeps the stderr default.

use crate::{ToolCall, ToolExecutor, ToolParam, ToolResult, ToolSchema, ToolTrustTier};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// The name of the notification tool, as exposed to the LLM.
pub const SEND_NOTIFICATION: &str = "send_notification";

/// One outbound notification: where it goes and the text it carries.
///
/// Rides the wire as the webhook transport's JSON body, so the fields
/// are the wire contract: `destination` names the chat, channel, or
/// webhook the host's transport understands; `message` is the text
/// delivered.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Notification {
    /// Where the notification goes — a destination the operator's
    /// transport understands (a chat id, a channel, a webhook name).
    pub destination: String,
    /// The notification text to deliver.
    pub message: String,
}

/// The transport seam — how a notification leaves the process.
///
/// Implementations are host-side: the chat driver's platform transport,
/// the CLI's webhook or stderr. `deliver` returns a human-readable
/// confirmation on success (rendered in the tool result), or the reason
/// it failed.
#[async_trait]
pub trait NotificationTransport: Send + Sync {
    /// Deliver one notification. `Ok` carries the confirmation string
    /// shown in the tool result; `Err` carries the delivery failure.
    async fn deliver(&self, notification: &Notification) -> Result<String, String>;
}

/// The default transport: write the notification to stderr.
///
/// The registry's default — every host that does not wire its own
/// transport still gets a working, honest delivery path (the operator's
/// terminal), never a silent no-op.
pub struct StderrTransport;

#[async_trait]
impl NotificationTransport for StderrTransport {
    async fn deliver(&self, notification: &Notification) -> Result<String, String> {
        eprintln!(
            "[notification] to {}: {}",
            notification.destination, notification.message
        );
        Ok(format!("stderr ({})", notification.destination))
    }
}

/// A webhook transport: POST the notification as JSON to a URL.
///
/// The CLI's `--webhook-url` wiring. Any non-success status is a
/// delivery failure — the tool reports it, the agent sees it, and
/// nothing is silently dropped.
pub struct WebhookTransport {
    url: String,
    client: reqwest::Client,
}

impl WebhookTransport {
    /// Build a transport that POSTs to `url` with a 10-second timeout.
    pub fn new(url: String) -> Self {
        let client = reqwest::Client::builder()
            .user_agent("Amparo/0.1 (companion AI; amparo@localhost)")
            .timeout(Duration::from_secs(10))
            .build()
            .expect("static webhook client configuration");
        Self { url, client }
    }
}

#[async_trait]
impl NotificationTransport for WebhookTransport {
    async fn deliver(&self, notification: &Notification) -> Result<String, String> {
        let response = self
            .client
            .post(&self.url)
            .json(notification)
            .send()
            .await
            .map_err(|e| format!("webhook delivery failed: {e}"))?;
        let status = response.status();
        if status.is_success() {
            Ok(format!("webhook {} (HTTP {})", self.url, status.as_u16()))
        } else {
            let body = response.text().await.unwrap_or_default();
            let detail = if body.trim().is_empty() {
                String::new()
            } else {
                format!(": {}", truncate_webhook_body(&body))
            };
            Err(format!(
                "webhook {} rejected the notification (HTTP {}){detail}",
                self.url,
                status.as_u16()
            ))
        }
    }
}

/// Keep an error body out of the result line: the first line is enough
/// for the agent to see what happened.
fn truncate_webhook_body(body: &str) -> String {
    let first_line = body.lines().next().unwrap_or("").trim();
    let mut out = first_line.to_string();
    if out.chars().count() > 200 {
        out = out.chars().take(200).collect();
        out.push('…');
    }
    out
}

/// The `send_notification` tool — delivers one message through the
/// host's configured [`NotificationTransport`].
pub struct SendNotificationTool {
    transport: Arc<dyn NotificationTransport>,
}

impl SendNotificationTool {
    /// Build the tool over an injected transport (the host's seam).
    pub fn new(transport: Arc<dyn NotificationTransport>) -> Self {
        Self { transport }
    }

    /// The registry default: deliver to stderr.
    pub fn to_stderr() -> Self {
        Self::new(Arc::new(StderrTransport))
    }

    /// The CLI wiring: deliver by POSTing to a webhook.
    pub fn to_webhook(url: String) -> Self {
        Self::new(Arc::new(WebhookTransport::new(url)))
    }
}

#[async_trait]
impl ToolExecutor for SendNotificationTool {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: SEND_NOTIFICATION.to_string(),
            description: "Send a notification to a destination outside this \
                 conversation — a chat, a channel, or a webhook the operator's \
                 transport knows. `destination` names where the message goes \
                 (the host's transport resolves it); `message` is the text \
                 delivered. The result confirms what was sent where. Sending \
                 reaches outside the workspace, so every send asks for human \
                 approval — the approval prompt shows the destination."
                .to_string(),
            parameters: vec![
                ToolParam {
                    name: "destination".to_string(),
                    description: "Where the notification goes — a destination \
                                  the operator's transport understands (a chat \
                                  id, a channel name, a webhook)."
                        .to_string(),
                    param_type: "string".to_string(),
                    enum_values: None,
                    required: true,
                },
                ToolParam {
                    name: "message".to_string(),
                    description: "The notification text to deliver.".to_string(),
                    param_type: "string".to_string(),
                    enum_values: None,
                    required: true,
                },
            ],
            trust_tier: ToolTrustTier::ExternalEffector,
        }
    }

    async fn execute(&self, call: &ToolCall) -> ToolResult {
        let start = Instant::now();

        let Some(destination) = call
            .arg_str("destination")
            .map(str::trim)
            .filter(|s| !s.is_empty())
        else {
            return failure(
                call,
                "send_notification requires a non-empty destination string parameter",
                start,
            );
        };
        let Some(message) = call
            .arg_str("message")
            .map(str::trim)
            .filter(|s| !s.is_empty())
        else {
            return failure(
                call,
                "send_notification requires a non-empty message string parameter",
                start,
            );
        };

        let notification = Notification {
            destination: destination.to_string(),
            message: message.to_string(),
        };

        match self.transport.deliver(&notification).await {
            Ok(confirmation) => ToolResult {
                tool_call_id: call.id.clone(),
                tool_name: call.name.clone(),
                success: true,
                output: serde_json::json!({
                    "destination": notification.destination,
                    "delivered": confirmation,
                }),
                display_summary: format!(
                    "notification to '{}': {}",
                    notification.destination, confirmation
                ),
                duration_ms: start.elapsed().as_millis() as u64,
            },
            Err(e) => failure(
                call,
                &format!("notification to '{}' failed: {e}", notification.destination),
                start,
            ),
        }
    }
}

/// Build the failed [`ToolResult`] shared by every error path.
fn failure(call: &ToolCall, message: &str, start: Instant) -> ToolResult {
    ToolResult {
        tool_call_id: call.id.clone(),
        tool_name: call.name.clone(),
        success: false,
        output: serde_json::json!({ "error": message }),
        display_summary: message.to_string(),
        duration_ms: start.elapsed().as_millis() as u64,
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::sync::Mutex;

    /// A recording transport: remembers every notification, answers
    /// with a canned confirmation.
    #[derive(Default)]
    struct RecordingTransport {
        seen: Mutex<Vec<Notification>>,
        fail_with: Mutex<Option<String>>,
    }

    #[async_trait]
    impl NotificationTransport for RecordingTransport {
        async fn deliver(&self, notification: &Notification) -> Result<String, String> {
            self.seen.lock().unwrap().push(notification.clone());
            match self.fail_with.lock().unwrap().clone() {
                Some(reason) => Err(reason),
                None => Ok("recorded (test transport)".to_string()),
            }
        }
    }

    fn call(arguments: serde_json::Value) -> ToolCall {
        ToolCall {
            id: "1".to_string(),
            name: SEND_NOTIFICATION.to_string(),
            arguments,
        }
    }

    #[test]
    fn schema_is_external_effector_and_names_the_destination() {
        let schema = SendNotificationTool::to_stderr().schema();
        assert_eq!(schema.name, SEND_NOTIFICATION);
        assert_eq!(schema.trust_tier, ToolTrustTier::ExternalEffector);
        assert_eq!(schema.parameters.len(), 2);
        for contract in ["destination", "approval", "message"] {
            assert!(
                schema.description.contains(contract),
                "description must state the contract ({contract}): {}",
                schema.description
            );
        }
        assert!(schema.parameters[0].required);
        assert!(schema.parameters[1].required);
    }

    #[tokio::test]
    async fn execute_delivers_through_the_transport() {
        let transport = Arc::new(RecordingTransport::default());
        let tool = SendNotificationTool::new(transport.clone());
        let result = tool
            .execute(&call(serde_json::json!({
                "destination": "ops-channel",
                "message": "deploy finished",
            })))
            .await;
        assert!(result.success, "delivery must succeed: {result:?}");
        assert_eq!(result.output["destination"], "ops-channel");
        assert_eq!(result.output["delivered"], "recorded (test transport)");
        let seen = transport.seen.lock().unwrap();
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].destination, "ops-channel");
        assert_eq!(seen[0].message, "deploy finished");
    }

    #[tokio::test]
    async fn execute_fails_without_a_destination() {
        let result = SendNotificationTool::to_stderr()
            .execute(&call(serde_json::json!({"message": "hello"})))
            .await;
        assert!(!result.success);
        assert!(result.output["error"]
            .as_str()
            .unwrap()
            .contains("destination"));
    }

    #[tokio::test]
    async fn execute_fails_without_a_message() {
        let result = SendNotificationTool::to_stderr()
            .execute(&call(serde_json::json!({"destination": "ops-channel"})))
            .await;
        assert!(!result.success);
        assert!(result.output["error"].as_str().unwrap().contains("message"));
    }

    #[tokio::test]
    async fn transport_failure_surfaces_as_a_failed_result() {
        let transport = Arc::new(RecordingTransport {
            seen: Mutex::new(Vec::new()),
            fail_with: Mutex::new(Some("chat unreachable".to_string())),
        });
        let result = SendNotificationTool::new(transport)
            .execute(&call(serde_json::json!({
                "destination": "ops-channel",
                "message": "deploy finished",
            })))
            .await;
        assert!(!result.success, "a transport failure must fail the call");
        let error = result.output["error"].as_str().unwrap();
        assert!(error.contains("chat unreachable"), "error: {error}");
        assert!(
            error.contains("ops-channel"),
            "error must name the destination"
        );
    }

    /// The default registry wires the stderr transport, so
    /// `send_notification` works everywhere without host wiring.
    #[tokio::test]
    async fn default_registry_dispatches_through_stderr() {
        let registry = crate::registry::default_registry();
        let result = registry
            .dispatch(&call(serde_json::json!({
                "destination": "operator",
                "message": "run complete",
            })))
            .await
            .expect("send_notification must be registered by default");
        assert!(result.success, "output: {}", result.output);
        assert_eq!(
            result.output["delivered"], "stderr (operator)",
            "the default transport is stderr"
        );
    }

    /// A hand-rolled HTTP responder (the MockPolicy accept-loop shape):
    /// accepts one request, captures it, answers 200.
    fn mock_webhook_server() -> (String, std::thread::JoinHandle<String>) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");
        let url = format!("http://{addr}/notify");
        let handle = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("one request");
            let mut buffer = [0u8; 8192];
            let read = stream.read(&mut buffer).expect("read request");
            let request = String::from_utf8_lossy(&buffer[..read]).to_string();
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                .expect("write response");
            request
        });
        (url, handle)
    }

    #[tokio::test]
    async fn webhook_transport_posts_the_notification_json() {
        let (url, handle) = mock_webhook_server();
        let tool = SendNotificationTool::to_webhook(url.clone());
        let result = tool
            .execute(&call(serde_json::json!({
                "destination": "ops-channel",
                "message": "deploy finished",
            })))
            .await;
        assert!(result.success, "webhook delivery must succeed: {result:?}");
        assert!(result.output["delivered"]
            .as_str()
            .unwrap()
            .contains("HTTP 200"));
        let request = handle.join().expect("responder thread");
        assert!(
            request.contains("POST /notify HTTP/1.1"),
            "must POST to the configured path: {request}"
        );
        assert!(
            request.contains("ops-channel"),
            "body must carry the destination"
        );
        assert!(
            request.contains("deploy finished"),
            "body must carry the message"
        );
        assert!(
            request.contains("\"destination\""),
            "body is the Notification JSON"
        );
    }

    #[tokio::test]
    async fn webhook_transport_reports_a_non_success_status() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");
        let url = format!("http://{addr}/notify");
        let responder = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("one request");
            let mut buffer = [0u8; 8192];
            let _ = stream.read(&mut buffer).expect("read request");
            stream
                .write_all(
                    b"HTTP/1.1 502 Bad Gateway\r\nContent-Length: 12\r\nConnection: close\r\n\r\nupstream out",
                )
                .expect("write response");
        });
        let tool = SendNotificationTool::to_webhook(url);
        let result = tool
            .execute(&call(serde_json::json!({
                "destination": "ops-channel",
                "message": "deploy finished",
            })))
            .await;
        responder.join().expect("responder thread");
        assert!(!result.success, "a 502 must fail the call");
        let error = result.output["error"].as_str().unwrap();
        assert!(
            error.contains("502"),
            "error must carry the status: {error}"
        );
        assert!(
            error.contains("upstream out"),
            "error must carry the body: {error}"
        );
    }
}
