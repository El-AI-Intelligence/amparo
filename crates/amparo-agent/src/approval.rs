//! The human-approval gate.
//!
//! Two things route a tool call here: a tool at trust tier ≥
//! `ExternalEffector` (it can reach outside the process), or a policy
//! `Escalate` verdict (the policy engine asks for a human decision).
//!
//! Axiom waited 60 seconds for a human and auto-denied on timeout. Amparo
//! keeps that behavior as the gate's *contract*: implementations ask a real
//! person however they like (console UI, CLI prompt, remote approver), and
//! should auto-deny rather than hang the agent. Amparo's built-in gates are
//! the two endpoints of that contract — [`AutoDeny`] (the default: no wired
//! human, no execution) and [`AutoApprove`] (the operator has deliberately
//! chosen unattended execution).

use amparo_tools::RollbackSpec;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::preflight::BlastRadius;

/// A request for human approval before a tool executes.
///
/// Serialization is the web-approval wire shape (M10 W4,
/// `docs/web-surface.md` §3): the gate POSTs exactly these fields —
/// `call_id`, `tool_name`, `arguments`, `reasons`, `blast_radius`,
/// `session_label`, `rollback` — to the approvals endpoint.
///
/// Deserialization accepts that same shape back (R3b: the Telegram
/// receiver parses the hub's listing entries into a request so the
/// approval copy renders identically on every surface). Missing optional
/// fields are `None` and fields the hub adds alongside (status, timestamps)
/// are ignored.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApprovalRequest {
    /// The tool call's id — echoed back in the tool-role answer.
    pub call_id: String,
    /// The tool the model wants to run.
    pub tool_name: String,
    /// The arguments it wants to run it with.
    pub arguments: Value,
    /// Why approval is required — policy escalation reasons, the trust
    /// tier, or both.
    pub reasons: Vec<String>,
    /// The preflight blast-radius classification (M7): what executing
    /// this call could touch, for the approval copy. `None` at the
    /// non-agent construction sites, which compute no classification —
    /// display-only context, never a gate input (I1).
    pub blast_radius: Option<BlastRadius>,
    /// The optional session label (M8): who is asking — `sub-agent
    /// sess-123.1 of task sess-123` — so a human approver sees the
    /// delegation chain behind the call. `None` for a top-level agent
    /// and at the non-agent construction sites. Display-only, like
    /// [`ApprovalRequest::blast_radius`] — never a gate input (I1).
    pub session_label: Option<String>,
    /// The tool-declared rollback hint (M10 W3): the idempotent undo
    /// path for this call, with any file-backup markers the tool
    /// created. Computed against the *pre-call* state and carried so
    /// the approval copy can show the human how to undo the call.
    /// Display-only, like [`ApprovalRequest::blast_radius`] — Amparo
    /// never executes a rollback itself (that would be auto-policy,
    /// I1).
    pub rollback: Option<RollbackSpec>,
}

/// The approval seam.
#[async_trait]
pub trait ApprovalGate: Send + Sync {
    /// Ask a human whether `request` may execute. `true` = approved.
    ///
    /// Implementations should return within a bounded time (Axiom used a
    /// 60-second auto-deny) so one pending decision cannot hang the agent.
    async fn request(&self, request: &ApprovalRequest) -> bool;
}

/// Always approves. Use only where the operator has deliberately chosen
/// unattended execution.
pub struct AutoApprove;

#[async_trait]
impl ApprovalGate for AutoApprove {
    async fn request(&self, _request: &ApprovalRequest) -> bool {
        true
    }
}

/// Always denies — the default gate. No wired human means no execution;
/// this matches the deny-by-default posture of the whole crate.
pub struct AutoDeny;

#[async_trait]
impl ApprovalGate for AutoDeny {
    async fn request(&self, _request: &ApprovalRequest) -> bool {
        false
    }
}
