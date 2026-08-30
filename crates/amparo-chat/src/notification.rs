//! The chat wiring for `send_notification` (M10 W2).
//!
//! [`ChatNotificationTransport`] adapts the platform's
//! [`crate::transport::ChatTransport`] to the tool's
//! [`amparo_tools::NotificationTransport`] seam: a notification's
//! `destination` is a chat id the provider can reach, and the message is
//! sent there as plain text. The tool itself stays platform-neutral —
//! this adapter is the only chat-specific piece.
//!
//! The destination is *not* restricted to the task's own chat: the tool
//! is [`amparo_tools::ToolTrustTier::ExternalEffector`], so every send
//! asks for human approval first, and the approval copy carries the
//! call's arguments — the human approves a named destination, never a
//! blanket "send something somewhere".

use crate::transport::{ChatRef, ChatTransport};
use amparo_tools::{Notification, NotificationTransport};
use async_trait::async_trait;
use std::sync::Arc;

/// Delivers a notification by routing it to the named chat over the
/// platform transport.
pub struct ChatNotificationTransport {
    transport: Arc<dyn ChatTransport>,
    /// The adapter's platform name (`"telegram"`, `"discord"`, `"slack"`).
    platform: &'static str,
}

impl ChatNotificationTransport {
    /// Build the adapter over a platform transport.
    ///
    /// `platform` fills the [`ChatRef::platform`] field; the user id is
    /// not needed for outbound sends, so it is left empty.
    pub fn new(transport: Arc<dyn ChatTransport>, platform: &'static str) -> Self {
        Self {
            transport,
            platform,
        }
    }
}

#[async_trait]
impl NotificationTransport for ChatNotificationTransport {
    async fn deliver(&self, notification: &Notification) -> Result<String, String> {
        let chat = ChatRef {
            platform: self.platform,
            chat_id: notification.destination.clone(),
            user_id: String::new(),
        };
        self.transport
            .send_text(&chat, &notification.message)
            .await
            .map(|_| {
                format!(
                    "sent to {} chat {}",
                    self.platform, notification.destination
                )
            })
            .map_err(|e| e.to_string())
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    #[allow(dead_code)]
    mod common {
        include!("../tests/common/mod.rs");
    }

    use super::*;
    use common::MockTransport;

    #[tokio::test]
    async fn deliver_routes_to_the_named_chat() {
        let mock = MockTransport::new();
        let adapter = ChatNotificationTransport::new(mock.clone(), "mock");
        let confirmation = adapter
            .deliver(&Notification {
                destination: "chat_42".to_string(),
                message: "deploy finished".to_string(),
            })
            .await
            .expect("delivery must succeed");
        assert_eq!(confirmation, "sent to mock chat chat_42");
        // The mock records (chat_id, text) pairs; assert the route.
        let chat = mock.last_chat().expect("one send");
        assert_eq!(chat.chat_id, "chat_42");
        assert_eq!(chat.user_id, "", "outbound sends carry no user id");
        assert_eq!(mock.texts(), vec!["deploy finished"]);
    }

    #[tokio::test]
    async fn a_failing_transport_fails_the_delivery() {
        let mock = MockTransport::new();
        mock.fail_next_text
            .store(true, std::sync::atomic::Ordering::SeqCst);
        let adapter = ChatNotificationTransport::new(mock, "mock");
        let result = adapter
            .deliver(&Notification {
                destination: "chat_42".to_string(),
                message: "hello".to_string(),
            })
            .await;
        assert!(result.is_err(), "a transport failure must surface");
    }
}
