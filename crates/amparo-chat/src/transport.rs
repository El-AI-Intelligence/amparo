//! The transport seam — what every chat platform adapter implements.
//!
//! A transport is dumb and reliable: it can send a text line, send an
//! approval message with inline buttons, edit that message after the human
//! decides, and run a receive loop that feeds a [`crate::driver::ChatDriver`].
//! Platform-specific wire types (Telegram's `Update`, Discord's
//! `MessageCreate`, Slack's `Envelope`) stay inside each adapter; everything
//! the driver and the gate see is one of the normalized types here.
//!
//! [`ChatTransport::receive`] is how a receive loop starts: each adapter
//! calls `driver.on_message` / `driver.on_approval` as its wire events
//! arrive. [`drain_outbox`] is the matching outbound helper — a per-task
//! event channel is drained into `send_text` by a spawned task.

use crate::driver::ChatDriver;
use amparo_agent::ApprovalRequest;
use async_trait::async_trait;
use std::sync::Arc;
use thiserror::Error;

/// Where a message came from and where replies go.
///
/// `platform` names the adapter (`"telegram"`, `"discord"`, `"slack"`);
/// `chat_id` and `user_id` are the platform's ids, kept as strings so every
/// adapter maps to the same shape.
#[derive(Debug, Clone)]
pub struct ChatRef {
    /// The adapter's platform name — `"telegram"`, `"discord"`, `"slack"`.
    pub platform: &'static str,
    /// The platform's id for the chat (DM or group).
    pub chat_id: String,
    /// The platform's id for the user who sent the message.
    pub user_id: String,
}

/// A normalized inbound message: which chat and user, plus the text.
#[derive(Debug, Clone)]
pub struct IncomingMessage {
    /// Where the message came from — the chat and the user in it.
    pub chat: ChatRef,
    /// The message text.
    pub text: String,
}

/// A normalized inline-button press: the chat, the presser, which approval,
/// and the decision.
#[derive(Debug, Clone)]
pub struct ApprovalButtonPress {
    /// The chat the approval message was sent to.
    pub chat_id: String,
    /// The approval id the button payload carried (the tool call id).
    pub approval_id: String,
    /// Whether the human pressed Approve (`true`) or Deny (`false`).
    pub approved: bool,
    /// The platform id of the user who pressed.
    pub user_id: String,
}

/// The outcome of routing one button press to the waiting gate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PressOutcome {
    /// The press reached the waiting gate and the decision was delivered.
    Routed,
    /// No pending approval — double press, timeout, or never registered.
    AlreadyDecided,
    /// A pending approval exists, but a different user pressed.
    WrongUser,
}

/// A platform handle to an approval message — what
/// [`ChatTransport::edit_approval`] edits. `message_id` is opaque to this
/// crate; each adapter knows how to address its own messages with it.
#[derive(Debug, Clone)]
pub struct ApprovalMessage {
    /// The chat the approval message lives in.
    pub chat_id: String,
    /// The platform's message id, used to edit it later.
    pub message_id: String,
}

/// The one-line M7 preflight note for an approval message, or `None`
/// when the host computed no classification. Every adapter splices this
/// into its approval copy so the human approves a concrete consequence,
/// not an abstraction.
pub fn preflight_line(request: &ApprovalRequest) -> Option<String> {
    request
        .blast_radius
        .as_ref()
        .map(|radius| format!("[preflight] blast radius: {radius} — {}", radius.note()))
}

/// The one-line M8 delegation label for an approval message — who is
/// asking — or `None` for a top-level agent. Like [`preflight_line`],
/// display-only: the gate has already decided, and the label never
/// feeds it (I1).
pub fn session_line(request: &ApprovalRequest) -> Option<String> {
    request
        .session_label
        .as_ref()
        .map(|label| format!("[session] {label} wants to run:"))
}

/// The one-line M10 W3 rollback hint for an approval message — the
/// idempotent undo path with any backup markers — or `None` when the
/// tool declared no hint. Like [`preflight_line`], display-only: the
/// gate has already decided, and Amparo never executes the rollback
/// itself (I1).
pub fn rollback_line(request: &ApprovalRequest) -> Option<String> {
    request.rollback.as_ref().map(amparo_agent::format_rollback)
}

/// The seam every chat adapter implements.
///
/// The first three methods are outbound; [`receive`](ChatTransport::receive)
/// runs the inbound loop and is started once by the host (`amparo chat
/// <platform>`), returning only when the loop ends. A transport must be
/// `Send + Sync` — it is shared by the receive loop, every per-task driver
/// future, and every approval gate.
#[async_trait]
pub trait ChatTransport: Send + Sync {
    /// Send a plain text line to `chat`.
    async fn send_text(&self, chat: &ChatRef, text: &str) -> Result<(), ChatError>;
    /// Send an approval request as an inline Approve/Deny button pair and
    /// return the message handle the gate will edit with the outcome.
    async fn send_approval(
        &self,
        chat: &ChatRef,
        request: &ApprovalRequest,
        approval_id: &str,
    ) -> Result<ApprovalMessage, ChatError>;
    /// Edit an approval message to show `outcome` (`"Approved"`,
    /// `"Denied"`, or a denial reason) and remove its buttons.
    async fn edit_approval(&self, msg: &ApprovalMessage, outcome: &str) -> Result<(), ChatError>;
    /// Run the platform's receive loop, forwarding normalized messages and
    /// button presses to `driver`, until the loop ends.
    async fn receive(self: Arc<Self>, driver: Arc<ChatDriver>) -> Result<(), ChatError>;
}

/// A transport or adapter failure.
///
/// The platform variants carry the adapter's own error text; [`Fatal`](ChatError::Fatal)
/// marks a failure that should take the whole serve loop down (for example
/// a Telegram 409 — the token is being used from another process — or 401).
#[derive(Error, Debug)]
pub enum ChatError {
    /// The Telegram adapter failed.
    #[error("telegram transport error: {0}")]
    Telegram(String),
    /// The Discord adapter failed.
    #[error("discord transport error: {0}")]
    Discord(String),
    /// The Slack adapter failed.
    #[error("slack transport error: {0}")]
    Slack(String),
    /// A websocket failure (Discord gateway, Slack Socket Mode).
    #[error("websocket error: {0}")]
    Ws(String),
    /// An HTTP failure.
    #[error("http error: {0}")]
    Http(String),
    /// A fatal failure — the serve loop should exit rather than retry.
    #[error("fatal: {0}")]
    Fatal(String),
}

/// Drain a per-task outbox into the chat: one `send_text` per line, in
/// order. Returns `Err` on the first send failure — the caller decides
/// whether that is fatal; returns `Ok` when the channel closes.
pub async fn drain_outbox(
    transport: &Arc<dyn ChatTransport>,
    chat: ChatRef,
    mut rx: tokio::sync::mpsc::Receiver<String>,
) -> Result<(), ChatError> {
    while let Some(line) = rx.recv().await {
        transport.send_text(&chat, &line).await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #[allow(dead_code)]
    mod common {
        include!("../tests/common/mod.rs");
    }

    use super::*;
    use common::MockTransport;

    fn request(radius: Option<amparo_agent::BlastRadius>) -> ApprovalRequest {
        ApprovalRequest {
            call_id: "c1".into(),
            tool_name: "run_command".into(),
            arguments: serde_json::json!({}),
            reasons: vec![],
            blast_radius: radius,
            session_label: None,
            rollback: None,
        }
    }

    #[test]
    fn preflight_line_formats_the_classification() {
        assert_eq!(
            preflight_line(&request(Some(amparo_agent::BlastRadius::Destructive))).unwrap(),
            "[preflight] blast radius: destructive — matches a blocked destructive pattern"
        );
    }

    #[test]
    fn preflight_line_is_none_without_a_classification() {
        assert_eq!(preflight_line(&request(None)), None);
    }

    #[test]
    fn session_line_names_the_sub_agent_chain() {
        let mut request = request(Some(amparo_agent::BlastRadius::Network));
        request.session_label = Some("sub-agent sess-123.1 of task sess-123".into());
        assert_eq!(
            session_line(&request).unwrap(),
            "[session] sub-agent sess-123.1 of task sess-123 wants to run:"
        );
    }

    #[test]
    fn session_line_is_none_for_a_top_level_agent() {
        assert_eq!(session_line(&request(None)), None);
    }

    #[test]
    fn rollback_line_formats_the_undo_with_the_marker() {
        let mut request = request(None);
        request.rollback = Some(amparo_tools::RollbackSpec {
            undo: "restore the previous contents of note.txt".into(),
            markers: vec!["note.txt.amparo-bak".into()],
        });
        assert_eq!(
            rollback_line(&request).unwrap(),
            "[rollback] restore the previous contents of note.txt \
             (backup: note.txt.amparo-bak)"
        );
    }

    #[test]
    fn rollback_line_is_none_without_a_hint() {
        assert_eq!(rollback_line(&request(None)), None);
    }

    #[tokio::test]
    async fn drain_outbox_delivers_lines_in_order() {
        let transport = MockTransport::new();
        let (tx, rx) = tokio::sync::mpsc::channel(4);
        tx.send("first line".to_string()).await.unwrap();
        tx.send("second line".to_string()).await.unwrap();
        drop(tx);
        let chat = ChatRef {
            platform: "mock",
            chat_id: "chat_1".into(),
            user_id: "user_1".into(),
        };
        let transport_trait: Arc<dyn ChatTransport> = transport.clone();
        drain_outbox(&transport_trait, chat, rx).await.unwrap();
        assert_eq!(transport.texts(), vec!["first line", "second line"]);
    }
}
