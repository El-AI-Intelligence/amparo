//! The chat approval gate — asks a human with inline buttons, auto-denies.
//!
//! The agent's [`ApprovalGate`] contract: ask, and return within a bounded
//! time so one pending decision cannot hang the loop. This gate asks via
//! [`ChatTransport::send_approval`] (an inline Approve/Deny button pair),
//! waits up to [`APPROVAL_TIMEOUT`] for the press routed back through the
//! [`ApprovalRouter`], and edits the message to record the outcome. Every
//! path is fail-closed: no transport, no press, no decision → denied.
//!
//! The gate is the single editor of its approval message — no other code
//! calls [`ChatTransport::edit_approval`] on a message this gate sent.

use crate::router::ApprovalRouter;
use crate::transport::{ChatRef, ChatTransport};
use amparo_agent::{ApprovalGate, ApprovalRequest};
use async_trait::async_trait;
use std::sync::Arc;
use std::time::Duration;

/// How long the gate waits for a button press before auto-denying.
pub const APPROVAL_TIMEOUT: Duration = Duration::from_secs(60);

/// The chat approval gate: ask via inline buttons, auto-deny on timeout.
///
/// Approval is scoped to one [`ChatRef`] — a gate is built per chat task
/// (the driver does this), so the buttons always land in the chat that is
/// waiting for them.
pub struct ChatApprovalGate {
    transport: Arc<dyn ChatTransport>,
    router: Arc<ApprovalRouter>,
    chat: ChatRef,
    auto_approve: bool,
    timeout: Duration,
}

impl ChatApprovalGate {
    /// Build a gate for `chat` that waits [`APPROVAL_TIMEOUT`] for a press.
    pub fn new(
        transport: Arc<dyn ChatTransport>,
        router: Arc<ApprovalRouter>,
        auto_approve: bool,
        chat: ChatRef,
    ) -> Self {
        Self {
            transport,
            router,
            chat,
            auto_approve,
            timeout: APPROVAL_TIMEOUT,
        }
    }

    /// Test-only constructor with a custom timeout, so the timeout path is
    /// testable without waiting 60 seconds.
    #[cfg(test)]
    pub fn with_timeout(
        transport: Arc<dyn ChatTransport>,
        router: Arc<ApprovalRouter>,
        auto_approve: bool,
        chat: ChatRef,
        timeout: Duration,
    ) -> Self {
        Self {
            transport,
            router,
            chat,
            auto_approve,
            timeout,
        }
    }
}

#[async_trait]
impl ApprovalGate for ChatApprovalGate {
    async fn request(&self, request: &ApprovalRequest) -> bool {
        if self.auto_approve {
            return true;
        }

        // The tool call id is the approval id: unique per task, one task
        // per chat, and short enough for every platform's button payload.
        let approval_id = request.call_id.clone();

        // Register before the message is sent: the entry must exist from
        // the first moment a press can possibly be delivered, or a press
        // landing in the send's wake finds no entry and is consumed as
        // already-decided — the decision is lost and the gate auto-denies
        // on its 60 s timeout (observed on slow CI runners, where the
        // receive loop's next poll can land between the send's completion
        // and the registration). A press cannot outrun its buttons: the
        // transport only delivers callbacks for messages that exist, and
        // the test mock holds the press until the keyboard message is
        // logged. The chat's user is the requester: only their press may
        // decide this approval.
        let rx = self
            .router
            .register(&self.chat.chat_id, &approval_id, &self.chat.user_id)
            .await;

        let Ok(msg) = self
            .transport
            .send_approval(&self.chat, request, &approval_id)
            .await
        else {
            // No message was sent — nothing to edit, fail closed. Remove
            // the entry registered above so no stale entry outlives this
            // ask (a later press finds nothing: already decided).
            self.router
                .unregister(&self.chat.chat_id, &approval_id)
                .await;
            return false;
        };

        match tokio::time::timeout(self.timeout, rx).await {
            Ok(Ok(true)) => {
                let _ = self.transport.edit_approval(&msg, "Approved").await;
                true
            }
            Ok(Ok(false)) => {
                let _ = self.transport.edit_approval(&msg, "Denied").await;
                false
            }
            Ok(Err(_)) => {
                // The sender vanished without a decision — a newer approval
                // superseded this one. The message is edited to say so.
                let _ = self
                    .transport
                    .edit_approval(&msg, "Denied — superseded")
                    .await;
                false
            }
            Err(_) => {
                // Timeout: unregister so a late press is "already decided"
                // rather than waking a dead channel, then record the
                // auto-deny on the message (this gate is its single editor).
                eprintln!(
                    "amparo chat gate: approval {approval_id} timed out in chat {}",
                    self.chat.chat_id
                );
                self.router
                    .unregister(&self.chat.chat_id, &approval_id)
                    .await;
                let _ = self
                    .transport
                    .edit_approval(&msg, "Denied — no response in time")
                    .await;
                false
            }
        }
    }
}

/// The timeout wrapper for a scheduled fire's approval gate (M8 W5).
///
/// The wrapped [`ChatApprovalGate`] already auto-denies on its own 60 s
/// timeout; this wrapper is belt-and-braces — a fire while nobody is
/// present runs to the gate and auto-denies here, never silently ahead
/// of it. On expiry it returns `false` (denied); the inner gate's own
/// timeout unregisters its router entry in the usual way.
pub struct TimeoutApprovalGate {
    /// The gate that actually asks (and records the outcome).
    inner: Arc<dyn ApprovalGate>,
    /// How long a scheduled fire may wait for a human.
    timeout: Duration,
}

impl TimeoutApprovalGate {
    /// Wrap `inner` with a `timeout`-bounded ask.
    pub fn new(inner: Arc<dyn ApprovalGate>, timeout: Duration) -> Self {
        Self { inner, timeout }
    }
}

#[async_trait]
impl ApprovalGate for TimeoutApprovalGate {
    async fn request(&self, request: &ApprovalRequest) -> bool {
        match tokio::time::timeout(self.timeout, self.inner.request(request)).await {
            Ok(approved) => approved,
            Err(_) => false, // nobody answered in time — fail closed
        }
    }
}

#[cfg(test)]
mod tests {
    #[allow(dead_code)]
    mod common {
        include!("../tests/common/mod.rs");
    }

    use super::*;
    use amparo_agent::{ApprovalRequest, BlastRadius};
    use common::{wait_until, MockTransport};
    use std::sync::atomic::Ordering;
    use std::time::Duration;

    fn chat() -> ChatRef {
        ChatRef {
            platform: "mock",
            chat_id: "chat_1".into(),
            user_id: "user_1".into(),
        }
    }

    fn request() -> ApprovalRequest {
        ApprovalRequest {
            call_id: "call_1".into(),
            tool_name: "run_command".into(),
            arguments: serde_json::json!({"command": "ls"}),
            reasons: vec!["tool tier external_effector requires human approval".into()],
            blast_radius: Some(BlastRadius::Network),
            session_label: None,
            rollback: None,
        }
    }

    fn gate(
        transport: Arc<MockTransport>,
        router: Arc<ApprovalRouter>,
        auto_approve: bool,
        timeout: Duration,
    ) -> ChatApprovalGate {
        ChatApprovalGate::with_timeout(transport, router, auto_approve, chat(), timeout)
    }

    #[tokio::test]
    async fn auto_approve_skips_the_message() {
        let transport = MockTransport::new();
        let gate = gate(
            transport.clone(),
            Arc::new(ApprovalRouter::new()),
            true,
            Duration::from_secs(5),
        );
        assert!(gate.request(&request()).await);
        assert!(
            transport.approvals().is_empty(),
            "auto-approve must not send a message"
        );
        assert!(transport.edits().is_empty());
    }

    #[tokio::test]
    async fn timeout_denies_and_edits() {
        let transport = MockTransport::new();
        let router = Arc::new(ApprovalRouter::new());
        let gate = gate(
            transport.clone(),
            router.clone(),
            false,
            Duration::from_millis(50),
        );
        assert!(!gate.request(&request()).await);
        assert_eq!(
            transport.approvals().len(),
            1,
            "one approval message was sent"
        );
        assert!(
            transport
                .edits()
                .iter()
                .any(|(_, outcome)| outcome == "Denied — no response in time"),
            "timeout must edit the message: {:?}",
            transport.edits()
        );
        // The entry was unregistered, so a late press is already decided.
        assert!(matches!(
            router.take("chat_1", "call_1", "user_1").await,
            crate::router::TakeResult::AlreadyDecided
        ));
    }

    #[tokio::test]
    async fn approved_press_edits_and_returns_true() {
        let transport = MockTransport::new();
        let router = Arc::new(ApprovalRouter::new());
        let gate = Arc::new(gate(
            transport.clone(),
            router.clone(),
            false,
            Duration::from_secs(5),
        ));
        let g = Arc::clone(&gate);
        let handle = tokio::spawn(async move { g.request(&request()).await });
        wait_until(|| !transport.approvals().is_empty()).await;
        let tx = match router.take("chat_1", "call_1", "user_1").await {
            crate::router::TakeResult::Routed(tx) => tx,
            other => panic!("approval must be registered, got {other:?}"),
        };
        tx.send(true).unwrap();
        assert!(handle.await.unwrap(), "approved press must approve");
        assert!(transport
            .edits()
            .iter()
            .any(|(_, outcome)| outcome == "Approved"));
    }

    #[tokio::test]
    async fn denied_press_edits_and_returns_false() {
        let transport = MockTransport::new();
        let router = Arc::new(ApprovalRouter::new());
        let gate = Arc::new(gate(
            transport.clone(),
            router.clone(),
            false,
            Duration::from_secs(5),
        ));
        let g = Arc::clone(&gate);
        let handle = tokio::spawn(async move { g.request(&request()).await });
        wait_until(|| !transport.approvals().is_empty()).await;
        let tx = match router.take("chat_1", "call_1", "user_1").await {
            crate::router::TakeResult::Routed(tx) => tx,
            other => panic!("approval must be registered, got {other:?}"),
        };
        tx.send(false).unwrap();
        assert!(!handle.await.unwrap(), "denied press must deny");
        assert!(transport
            .edits()
            .iter()
            .any(|(_, outcome)| outcome == "Denied"));
    }

    #[tokio::test]
    async fn send_failure_denies_fail_closed() {
        let transport = MockTransport::new();
        let router = Arc::new(ApprovalRouter::new());
        let gate = gate(
            transport.clone(),
            router.clone(),
            false,
            Duration::from_secs(5),
        );
        transport.fail_next_send.store(true, Ordering::SeqCst);
        assert!(!gate.request(&request()).await, "send failure must deny");
        assert!(transport.approvals().is_empty(), "nothing was sent");
        assert!(transport.edits().is_empty(), "no message to edit");
        assert!(
            matches!(
                router.take("chat_1", "call_1", "user_1").await,
                crate::router::TakeResult::AlreadyDecided
            ),
            "nothing was registered"
        );
    }

    /// A gate that sleeps `sleep` and then answers `answer` — for testing
    /// the timeout wrapper without touching a transport.
    struct SleepyGate {
        sleep: Duration,
        answer: bool,
    }

    #[async_trait]
    impl ApprovalGate for SleepyGate {
        async fn request(&self, _request: &ApprovalRequest) -> bool {
            tokio::time::sleep(self.sleep).await;
            self.answer
        }
    }

    #[tokio::test]
    async fn timeout_wrapper_denies_when_the_inner_gate_stalls() {
        let sleepy = Arc::new(SleepyGate {
            sleep: Duration::from_millis(100),
            answer: true,
        });
        let wrapped = TimeoutApprovalGate::new(sleepy, Duration::from_millis(10));
        assert!(
            !wrapped.request(&request()).await,
            "a stalled gate must deny, never hang"
        );
    }

    #[tokio::test]
    async fn timeout_wrapper_passes_the_inner_verdict_through() {
        let approve = Arc::new(SleepyGate {
            sleep: Duration::from_millis(1),
            answer: true,
        });
        assert!(
            TimeoutApprovalGate::new(approve, Duration::from_secs(5))
                .request(&request())
                .await,
            "a fast inner approval must approve"
        );
        let deny = Arc::new(SleepyGate {
            sleep: Duration::from_millis(1),
            answer: false,
        });
        assert!(
            !TimeoutApprovalGate::new(deny, Duration::from_secs(5))
                .request(&request())
                .await,
            "a fast inner denial must deny"
        );
    }
}
