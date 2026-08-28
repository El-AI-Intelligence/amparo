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

/// A normalized inline-button press: the chat, which approval, and the
/// decision.
#[derive(Debug, Clone)]
pub struct ApprovalButtonPress {
    /// The chat the approval message was sent to.
    pub chat_id: String,
    /// The approval id the button payload carried (the tool call id).
    pub approval_id: String,
    /// Whether the human pressed Approve (`true`) or Deny (`false`).
    pub approved: bool,
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
