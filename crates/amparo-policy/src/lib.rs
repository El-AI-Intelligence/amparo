//! Amparo Policy — the seam between "the model decided to do this" and "this ran".
//!
//! Every tool call passes a policy check before it executes. Amparo itself ships
//! no judgment engine — it ships the *contract* and a deny-by-default posture:
//!
//! - [`PolicyEngine`] is the trait. Implementations swap behind one seam.
//! - [`DenyAllPolicyEngine`] is the default: an agent with no configured policy
//!   refuses to act. Deny-all is the only safe default — an agent whose policy
//!   waves everything through is worse than one with no policy engine, because
//!   it looks safe.
//! - [`wire::WirePolicyEngine`] speaks the open policy-check wire protocol
//!   (`POST /check {tool_name, target} → {verdict, reason, enforced}`), so a
//!   remote engine — [Guardrail](https://elai-intelligence.com) is the
//!   commercial implementation — plugs in over HTTP. Anyone can write another.
//! - [`AllowAllPolicyEngine`] is the explicit opt-in for local experiments.
//!   It never becomes the default — an operator has to name it.
//!
//! **Caller contract** (from the wire spec, implemented here):
//!
//! | Verdict | Behavior |
//! |---|---|
//! | Allow | Execute. |
//! | Deny (enforced) | Hard-block, with reasons surfaced. |
//! | Deny / Escalate (audit-only, `enforced: false`) | Do **not** block — the verdict is a prediction. Proceed, log the engine's real verdict. |
//! | Escalate (enforced) | Ask a human. Never execute silently. |
//! | Engine failure / timeout | Fail-safe **Escalate** — an engine error is never an allow. |
//! | `limit_reached: true` | Hard-block. This bypasses audit mode by design. |
//!
//! Ported from the axiom-daemon `policy_engine.rs` seam (Axiom-OS, MIT,
//! Copyright (c) Pixel Phantom AI); see the repository NOTICE. The ELLM bridge
//! stays behind — Amparo's seam speaks the open protocol instead.

#![warn(missing_docs)]

pub mod wire;

use async_trait::async_trait;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

/// A policy verdict for a single tool call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PolicyVerdict {
    /// Execute.
    Allow,
    /// Hard-block.
    Deny,
    /// Needs a human decision — never execute silently.
    Escalate,
}

impl PolicyVerdict {
    /// Wire/display form: `allowed` / `denied` / `escalated`.
    pub fn as_str(self) -> &'static str {
        match self {
            PolicyVerdict::Allow => "allowed",
            PolicyVerdict::Deny => "denied",
            PolicyVerdict::Escalate => "escalated",
        }
    }
}

/// The outcome of a policy check.
#[derive(Debug, Clone)]
pub struct PolicyDecision {
    /// The verdict for this tool call.
    pub verdict: PolicyVerdict,
    /// Human-readable rule firings explaining the verdict — denials must never
    /// be silent.
    pub fired: Vec<String>,
}

impl PolicyDecision {
    /// Builds an allow decision with no fired rules.
    pub fn allow() -> Self {
        Self {
            verdict: PolicyVerdict::Allow,
            fired: Vec::new(),
        }
    }

    /// Builds a deny decision carrying the given reason.
    pub fn deny(reason: impl Into<String>) -> Self {
        Self {
            verdict: PolicyVerdict::Deny,
            fired: vec![reason.into()],
        }
    }

    /// Builds an escalate decision carrying the given reason.
    pub fn escalate(reason: impl Into<String>) -> Self {
        Self {
            verdict: PolicyVerdict::Escalate,
            fired: vec![reason.into()],
        }
    }
}

/// The policy seam. Every tool call is judged through this before execution.
///
/// `tool_name` is the registry tool name (`run_command`, `write_file`, …),
/// `target` the primary argument (the shell command for `run_command`, the path
/// for file tools), `params` supplementary key/value pairs.
#[async_trait]
pub trait PolicyEngine: Send + Sync {
    /// Judges a single tool call and returns the decision the caller must enforce.
    async fn judge_tool(
        &self,
        tool_name: &str,
        target: &str,
        params: &[(&str, &str)],
    ) -> PolicyDecision;

    /// Whether this engine has answered in audit mode — verdicts
    /// advisory, not enforced.
    ///
    /// `false` by default: most engines never audit. [`AuditNoticeEngine`]
    /// overrides it to `true` once an underlying decision carried the
    /// audit-only marker — exactly when the one-time advisory notice
    /// fires. Hosts read this to recolor their policy status (the TUI's
    /// `§` symbol) without parsing verdicts themselves.
    fn audit_mode(&self) -> bool {
        false
    }
}

/// Fail-safe default engine: refuses everything, with the reason surfaced in
/// every decision so denials are legible.
#[derive(Debug, Clone)]
pub struct DenyAllPolicyEngine {
    reason: &'static str,
}

impl DenyAllPolicyEngine {
    /// Builds a deny-all engine whose decisions surface the given reason.
    pub fn new(reason: &'static str) -> Self {
        Self { reason }
    }
}

#[async_trait]
impl PolicyEngine for DenyAllPolicyEngine {
    async fn judge_tool(
        &self,
        tool_name: &str,
        _target: &str,
        _params: &[(&str, &str)],
    ) -> PolicyDecision {
        PolicyDecision::deny(format!("{}: {}", self.reason, tool_name))
    }
}

/// Explicit opt-in engine: allows every call, with no fired rules.
///
/// Never the default — wiring this in means the operator has decided no
/// policy checks are wanted (local experiments, sandboxed environments).
/// An agent with no configured policy must use [`DenyAllPolicyEngine`] —
/// allow-all only ever appears by name.
#[derive(Debug, Clone, Copy, Default)]
pub struct AllowAllPolicyEngine;

#[async_trait]
impl PolicyEngine for AllowAllPolicyEngine {
    async fn judge_tool(
        &self,
        _tool_name: &str,
        _target: &str,
        _params: &[(&str, &str)],
    ) -> PolicyDecision {
        PolicyDecision::allow()
    }
}

/// The exact one-time stderr notice when a wire engine first answers in
/// audit mode (`enforced: false`) — the copy promised at
/// `docs/trial-bundle.md` §"graceful degradation".
pub const AUDIT_NOTICE: &str = "policy engine is in audit mode; verdicts are advisory";

/// A [`PolicyEngine`] wrapper that prints [`AUDIT_NOTICE`] to stderr the
/// first time an underlying decision is audit-only — and never again.
///
/// The notice is display-only (I1): verdicts pass through untouched, so
/// audit mode stays advisory by contract. The flag is an `Arc` so one
/// process serving many engines (the chat driver's per-task wire engines)
/// shares a single notice; every other caller gets a fresh flag.
pub struct AuditNoticeEngine<E> {
    inner: E,
    noticed: Arc<AtomicBool>,
    print: fn(&str),
}

impl<E> AuditNoticeEngine<E> {
    /// Wraps `inner` with a fresh notice flag and the default stderr
    /// printer — one notice per process.
    pub fn new(inner: E) -> Self {
        Self::with_flag(inner, Arc::new(AtomicBool::new(false)))
    }

    /// Wraps `inner` sharing an existing notice flag: engines built for
    /// separate tasks in one process print the notice once, not once per
    /// task.
    pub fn with_flag(inner: E, noticed: Arc<AtomicBool>) -> Self {
        Self::with_printer(inner, noticed, |line| eprintln!("{line}"))
    }

    /// The test seam: wraps `inner` sharing `noticed`, printing through
    /// `print` instead of stderr.
    pub fn with_printer(inner: E, noticed: Arc<AtomicBool>, print: fn(&str)) -> Self {
        Self {
            inner,
            noticed,
            print,
        }
    }
}

#[async_trait]
impl<E: PolicyEngine> PolicyEngine for AuditNoticeEngine<E> {
    async fn judge_tool(
        &self,
        tool_name: &str,
        target: &str,
        params: &[(&str, &str)],
    ) -> PolicyDecision {
        let decision = self.inner.judge_tool(tool_name, target, params).await;
        if decision
            .fired
            .iter()
            .any(|f| f.contains(wire::AUDIT_ONLY_MARKER))
            && !self.noticed.swap(true, Ordering::SeqCst)
        {
            (self.print)(AUDIT_NOTICE);
        }
        decision
    }

    fn audit_mode(&self) -> bool {
        self.noticed.load(Ordering::SeqCst)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn deny_all_default_denies_everything_including_reads() {
        let engine = DenyAllPolicyEngine::new("test");
        let d = engine.judge_tool("read_file", "/tmp/x", &[]).await;
        assert_eq!(d.verdict, PolicyVerdict::Deny);
        assert!(!d.fired.is_empty(), "denials must carry a reason");

        let d = engine.judge_tool("run_command", "rm -rf /", &[]).await;
        assert_eq!(d.verdict, PolicyVerdict::Deny);
    }

    #[tokio::test]
    async fn deny_all_never_allows_or_escalates() {
        // The default posture is deny-all — even unclassified tools must not
        // slip through as Escalate-then-execute.
        let engine = DenyAllPolicyEngine::new("test");
        for tool in ["read_file", "run_command", "some_future_tool"] {
            let d = engine.judge_tool(tool, "anything", &[]).await;
            assert_eq!(d.verdict, PolicyVerdict::Deny, "{} must be denied", tool);
        }
    }

    #[tokio::test]
    async fn allow_all_engine_is_the_explicit_named_opt_in() {
        let engine = AllowAllPolicyEngine;
        let d = engine.judge_tool("run_command", "rm -rf /", &[]).await;
        assert_eq!(d.verdict, PolicyVerdict::Allow);
        assert!(d.fired.is_empty(), "allow-all fires no rules");
    }

    #[test]
    fn verdict_strings_are_stable_wire_values() {
        assert_eq!(PolicyVerdict::Allow.as_str(), "allowed");
        assert_eq!(PolicyVerdict::Deny.as_str(), "denied");
        assert_eq!(PolicyVerdict::Escalate.as_str(), "escalated");
    }

    // ── AuditNoticeEngine (M9 W3) ─────────────────────────────────────────

    use std::sync::atomic::AtomicUsize;

    /// A scripted inner engine: returns the given decisions in order, then
    /// allows.
    struct ScriptedEngine {
        decisions: Vec<PolicyDecision>,
        seen: std::sync::Mutex<usize>,
    }

    impl ScriptedEngine {
        fn new(decisions: Vec<PolicyDecision>) -> Self {
            Self {
                decisions,
                seen: std::sync::Mutex::new(0),
            }
        }
    }

    #[async_trait]
    impl PolicyEngine for ScriptedEngine {
        async fn judge_tool(
            &self,
            _tool_name: &str,
            _target: &str,
            _params: &[(&str, &str)],
        ) -> PolicyDecision {
            let mut seen = self.seen.lock().unwrap();
            let i = *seen;
            *seen += 1;
            self.decisions
                .get(i)
                .cloned()
                .unwrap_or_else(PolicyDecision::allow)
        }
    }

    fn audit_decision() -> PolicyDecision {
        PolicyDecision {
            verdict: PolicyVerdict::Allow,
            fired: vec![format!(
                "{} (enforced:false): engine verdict deny — test; proceeding unenforced",
                wire::AUDIT_ONLY_MARKER
            )],
        }
    }

    static PRINTS_ONCE: AtomicUsize = AtomicUsize::new(0);
    static LAST_LINE_ONCE: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);

    fn record_once(line: &str) {
        PRINTS_ONCE.fetch_add(1, Ordering::SeqCst);
        *LAST_LINE_ONCE.lock().unwrap() = Some(line.to_string());
    }

    #[tokio::test]
    async fn audit_notice_prints_exactly_once_with_the_exact_copy() {
        let inner = ScriptedEngine::new(vec![audit_decision(), audit_decision(), audit_decision()]);
        let engine =
            AuditNoticeEngine::with_printer(inner, Arc::new(AtomicBool::new(false)), record_once);
        for _ in 0..3 {
            let d = engine.judge_tool("run_command", "x", &[]).await;
            assert_eq!(
                d.verdict,
                PolicyVerdict::Allow,
                "verdicts pass through untouched"
            );
        }
        assert_eq!(
            PRINTS_ONCE.load(Ordering::SeqCst),
            1,
            "one notice, not three"
        );
        assert_eq!(
            LAST_LINE_ONCE.lock().unwrap().as_deref(),
            Some(AUDIT_NOTICE)
        );
    }

    static PRINTS_CLEAN: AtomicUsize = AtomicUsize::new(0);

    fn record_clean(_line: &str) {
        PRINTS_CLEAN.fetch_add(1, Ordering::SeqCst);
    }

    #[tokio::test]
    async fn non_audit_decisions_never_print_the_notice() {
        let inner = ScriptedEngine::new(vec![
            PolicyDecision::allow(),
            PolicyDecision::deny("enforced block"),
            PolicyDecision::escalate("engine failure: down"),
        ]);
        let engine =
            AuditNoticeEngine::with_printer(inner, Arc::new(AtomicBool::new(false)), record_clean);
        engine.judge_tool("read_file", "/tmp/x", &[]).await;
        engine.judge_tool("run_command", "rm -rf /", &[]).await;
        engine.judge_tool("write_file", "/tmp/y", &[]).await;
        assert_eq!(PRINTS_CLEAN.load(Ordering::SeqCst), 0);
    }

    static PRINTS_SHARED: AtomicUsize = AtomicUsize::new(0);

    fn record_shared(_line: &str) {
        PRINTS_SHARED.fetch_add(1, Ordering::SeqCst);
    }

    #[tokio::test]
    async fn one_shared_flag_prints_once_across_many_engines() {
        // The chat driver builds a fresh wire engine per task; one process
        // must still print the notice exactly once.
        let flag = Arc::new(AtomicBool::new(false));
        let first = AuditNoticeEngine::with_printer(
            ScriptedEngine::new(vec![audit_decision()]),
            Arc::clone(&flag),
            record_shared,
        );
        let second = AuditNoticeEngine::with_printer(
            ScriptedEngine::new(vec![audit_decision()]),
            Arc::clone(&flag),
            record_shared,
        );
        first.judge_tool("run_command", "a", &[]).await;
        second.judge_tool("run_command", "b", &[]).await;
        assert_eq!(
            PRINTS_SHARED.load(Ordering::SeqCst),
            1,
            "one process, one notice"
        );
    }
}
