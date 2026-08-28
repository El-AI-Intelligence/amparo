//! The chat driver — turns a normalized message into a per-task agent run.
//!
//! [`ChatDriver`] is the only thing transports talk to: `on_message` for a
//! text, `on_approval` for a button press. Everything heavy happens inside a
//! spawned per-task future, so a panicking agent or a hung inference call
//! can never take down a receive loop.

use crate::gate::ChatApprovalGate;
use crate::router::ApprovalRouter;
use crate::sink::ChatEventSink;
use crate::transport::{drain_outbox, ApprovalButtonPress, ChatRef, ChatTransport};
use amparo_agent::Agent;
use amparo_inference::InferenceProvider;
use amparo_policy::PolicyEngine;
use amparo_privacy::PrivacyPolicy;
use amparo_tools::ToolRegistry;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

/// A claim on a chat: while it lives, no second task may start in that
/// chat. Dropping it — however the task ends — releases the claim, so the
/// busy map can never leak a stuck entry.
struct ChatClaim {
    busy: Arc<Mutex<HashSet<String>>>,
    chat_id: String,
}

impl Drop for ChatClaim {
    fn drop(&mut self) {
        self.busy.lock().unwrap().remove(&self.chat_id);
    }
}

/// The chat driver: one per receive loop, the sole owner of task policy.
///
/// A message from an allowlisted user claims its chat (one task at a time),
/// and the task is a fresh [`amparo_agent::Agent`] built from the shared
/// parts — the same provider, policy engine and tool registry, with a
/// per-task event sink and a [`ChatApprovalGate`] bound to that chat. The
/// run happens inside its own `tokio::spawn`, so a panic surfaces as a
/// `JoinError` and the chat gets "The task crashed" instead of the loop
/// going down with it.
///
/// Fail-closed: an empty allowlist denies everyone (no message ever starts
/// a task), and the busy claim is released by a drop guard, never by
/// control flow that could be skipped.
///
/// M4 is single-operator: one shared policy engine, one shared registry,
/// one shared workspace. M5 makes policy per-user — the per-task Agent
/// build below is exactly the seam where a session-scoped engine
/// (`amparo_policy::wire::WirePolicyEngine::with_session_id(user_id)`)
/// slots in without touching anything here.
pub struct ChatDriver {
    /// User ids allowed to start tasks. An empty set denies everyone —
    /// `AMPARO_CHAT_ALLOWLIST` feeds this in dispatch, and absent or empty
    /// must mean "nobody".
    allowlist: HashSet<String>,
    /// The shared inference provider, shared by every task's agent.
    provider: Arc<dyn InferenceProvider>,
    /// The shared policy engine — one per driver in M4; per-user in M5
    /// (see the struct docs for the seam).
    policy: Arc<dyn PolicyEngine>,
    /// Optional privacy policy, attached to every task's agent.
    privacy: Option<Arc<PrivacyPolicy>>,
    /// The shared tool registry, cloned per task (tools are Send+Sync).
    registry: ToolRegistry,
    /// The workspace directory tasks operate in (M4: one shared workspace;
    /// M5 may scope it per user).
    workspace: PathBuf,
    /// Outbound text and approval messages go through the platform transport.
    transport: Arc<dyn ChatTransport>,
    /// Routes button presses back to the gate that is waiting for them.
    router: Arc<ApprovalRouter>,
    /// `true` = approvals succeed without asking (deliberate unattended
    /// mode — the equivalent of `--auto-approve`).
    auto_approve: bool,
    /// Chat ids with a task in flight. std Mutex, and never held across an
    /// await — claim and release are synchronous.
    busy: Arc<Mutex<HashSet<String>>>,
}

impl ChatDriver {
    /// Build a driver.
    ///
    /// `allowlist` is the set of `user_id`s allowed to start tasks — empty
    /// means nobody, and every message is refused. The gate parts
    /// (`transport`, `router`, `auto_approve`) build one
    /// [`ChatApprovalGate`] per task, bound to that task's chat.
    pub fn new(
        allowlist: HashSet<String>,
        provider: Arc<dyn InferenceProvider>,
        policy: Arc<dyn PolicyEngine>,
        registry: ToolRegistry,
        workspace: PathBuf,
        transport: Arc<dyn ChatTransport>,
        router: Arc<ApprovalRouter>,
        auto_approve: bool,
    ) -> Self {
        Self {
            allowlist,
            provider,
            policy,
            privacy: None,
            registry,
            workspace,
            transport,
            router,
            auto_approve,
            busy: Arc::new(Mutex::new(HashSet::new())),
        }
    }

    /// Attach a privacy policy to every task this driver runs.
    pub fn with_privacy(mut self, policy: Arc<PrivacyPolicy>) -> Self {
        self.privacy = Some(policy);
        self
    }

    /// Whether the user in `chat` may start tasks.
    ///
    /// M4: a flat user-id allowlist. M5 adds org/identity resolution here —
    /// the chat's `user_id` is already threaded through everywhere it is
    /// needed.
    pub fn allowlisted(&self, chat: &ChatRef) -> bool {
        self.allowlist.contains(&chat.user_id)
    }

    /// The workspace directory tasks operate in.
    pub fn workspace(&self) -> &Path {
        &self.workspace
    }

    /// Handle one inbound message.
    ///
    /// Not allowlisted → a polite refusal. Busy → a "still running" reply.
    /// Otherwise the chat is claimed and a per-task future is spawned that
    /// builds a fresh agent (shared parts, per-chat approval gate,
    /// per-task event sink), runs it inside its own spawn for panic
    /// containment, and delivers the final answer with a direct awaited
    /// `send_text` — bypassing the event sink so it can never be lost.
    pub async fn on_message(&self, chat: ChatRef, text: String) {
        if !self.allowlisted(&chat) {
            let _ = self
                .transport
                .send_text(&chat, "This chat is not authorized to use this agent.")
                .await;
            return;
        }

        {
            let mut busy = self.busy.lock().unwrap();
            if busy.contains(&chat.chat_id) {
                drop(busy);
                let _ = self
                    .transport
                    .send_text(&chat, "Another task is still running — wait for it to finish.")
                    .await;
                return;
            }
            busy.insert(chat.chat_id.clone());
        }

        let provider = Arc::clone(&self.provider);
        let policy = Arc::clone(&self.policy);
        let privacy = self.privacy.clone();
        let registry = self.registry.clone();
        let transport = Arc::clone(&self.transport);
        let router = Arc::clone(&self.router);
        let busy = Arc::clone(&self.busy);
        let auto_approve = self.auto_approve;

        tokio::spawn(async move {
            // The claim is the busy-map entry; dropping it (however this
            // task ends) releases the chat.
            let _claim = ChatClaim { busy, chat_id: chat.chat_id.clone() };

            // Progress events flow through a best-effort outbox; the final
            // answer below bypasses it.
            let (sink, rx) = ChatEventSink::channel();
            let drain_transport = Arc::clone(&transport);
            let drain_chat = chat.clone();
            tokio::spawn(async move {
                let _ = drain_outbox(&drain_transport, drain_chat, rx).await;
            });

            // A fresh agent per task: the approval gate is bound to this
            // chat, and the event sink to this task's outbox.
            let gate = Arc::new(ChatApprovalGate::new(
                Arc::clone(&transport),
                Arc::clone(&router),
                auto_approve,
                chat.clone(),
            ));
            let mut agent = Agent::new(Arc::clone(&provider), registry, Arc::clone(&policy))
                .with_events(sink)
                .with_approval(gate);
            if let Some(privacy) = privacy {
                agent = agent.with_privacy(privacy);
            }

            // Run inside its own spawn so a panic becomes a JoinError
            // instead of taking down this task — and the receive loop.
            let run = tokio::spawn(async move { agent.run(text).await });
            match run.await {
                Ok(report) => {
                    let answer = match report.final_answer {
                        Some(answer) => answer,
                        None => "The task failed — no final answer was produced.".to_string(),
                    };
                    let _ = transport.send_text(&chat, &answer).await;
                }
                Err(_) => {
                    let _ = transport.send_text(&chat, "The task crashed").await;
                }
            }
        });
    }

    /// Handle one inline-button press. Returns whether the press reached a
    /// waiting gate — `false` means already decided (or never registered),
    /// which the platform adapter reports as "already decided".
    pub async fn on_approval(&self, press: ApprovalButtonPress) -> bool {
        match self.router.take(&press.chat_id, &press.approval_id).await {
            Some(tx) => {
                // The gate may have timed out and dropped its receiver just
                // now — the press is still consumed (already decided), never
                // replayed.
                let _ = tx.send(press.approved);
                true
            }
            None => false,
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
    use amparo_policy::AllowAllPolicyEngine;
    use amparo_tools::ToolTrustTier;
    use common::{
        registry_with_echo, turn_text, turn_tool_call, wait_for_text, wait_until, MockTransport,
        StubProvider,
    };

    fn chat() -> ChatRef {
        ChatRef { platform: "mock", chat_id: "chat_1".into(), user_id: "user_1".into() }
    }

    fn driver_with(
        allowlist: HashSet<String>,
        transport: Arc<MockTransport>,
        provider: Arc<StubProvider>,
        auto_approve: bool,
        echo_tier: ToolTrustTier,
    ) -> ChatDriver {
        ChatDriver::new(
            allowlist,
            provider,
            Arc::new(AllowAllPolicyEngine),
            registry_with_echo(echo_tier),
            PathBuf::from("/tmp/amparo-chat-test"),
            transport,
            Arc::new(ApprovalRouter::new()),
            auto_approve,
        )
    }

    fn driver(
        transport: Arc<MockTransport>,
        provider: Arc<StubProvider>,
        auto_approve: bool,
    ) -> ChatDriver {
        driver_with(
            HashSet::from(["user_1".to_string()]),
            transport,
            provider,
            auto_approve,
            ToolTrustTier::Observational,
        )
    }

    #[tokio::test]
    async fn empty_allowlist_refuses_politely() {
        let transport = MockTransport::new();
        let driver = driver_with(
            HashSet::new(),
            transport.clone(),
            StubProvider::new(vec![]),
            true,
            ToolTrustTier::Observational,
        );
        driver.on_message(chat(), "do something".into()).await;
        let line = wait_for_text(&transport, "not authorized").await;
        assert!(line.starts_with("This chat is not authorized"), "{line}");
    }

    #[tokio::test]
    async fn allowlisted_user_gets_a_task() {
        let transport = MockTransport::new();
        let provider = StubProvider::new(vec![turn_text("The answer is 42.")]);
        let driver = driver(transport.clone(), provider, true);
        driver.on_message(chat(), "what is the answer?".into()).await;
        wait_for_text(&transport, "The answer is 42.").await;
    }

    #[tokio::test]
    async fn busy_chat_gets_the_busy_reply() {
        let transport = MockTransport::new();
        let driver = driver(transport.clone(), StubProvider::new(vec![]), true);
        driver.on_message(chat(), "first task".into()).await;
        driver.on_message(chat(), "second task".into()).await;
        wait_for_text(&transport, "Another task is still running").await;
    }

    #[tokio::test]
    async fn approval_press_routes_the_decision() {
        let transport = MockTransport::new();
        let provider = StubProvider::new(vec![
            turn_tool_call("call_1", "echo", r#"{"message":"hi"}"#),
            turn_text("Done."),
        ]);
        let driver = driver_with(
            HashSet::from(["user_1".to_string()]),
            transport.clone(),
            provider,
            false, // approvals must be pressed, not auto-granted
            ToolTrustTier::ExternalEffector, // tier forces the approval gate
        );
        driver.on_message(chat(), "do a thing".into()).await;
        wait_until(|| !transport.approvals().is_empty()).await;

        let press = ApprovalButtonPress {
            chat_id: "chat_1".into(),
            approval_id: "call_1".into(),
            approved: true,
        };
        assert!(driver.on_approval(press).await, "the press reached the waiting gate");
        wait_for_text(&transport, "Done.").await;
    }

    #[tokio::test]
    async fn unknown_press_is_already_decided() {
        let transport = MockTransport::new();
        let driver = driver(transport.clone(), StubProvider::new(vec![]), true);
        let press = ApprovalButtonPress {
            chat_id: "chat_1".into(),
            approval_id: "never_registered".into(),
            approved: true,
        };
        assert!(!driver.on_approval(press).await);
    }

    #[tokio::test]
    async fn provider_panic_sends_crash_notice() {
        let transport = MockTransport::new();
        let driver = driver(transport.clone(), StubProvider::panicking(), true);
        driver.on_message(chat(), "cause a panic".into()).await;
        wait_for_text(&transport, "The task crashed").await;
    }
}
