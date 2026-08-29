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

use async_trait::async_trait;
use serde_json::Value;

use crate::preflight::BlastRadius;

/// A request for human approval before a tool executes.
#[derive(Debug, Clone)]
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
