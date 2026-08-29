//! The chat driver — turns a normalized message into a per-task agent run.
//!
//! [`ChatDriver`] is the only thing transports talk to: `on_message` for a
//! text, `on_approval` for a button press. Everything heavy happens inside a
//! spawned per-task future, so a panicking agent or a hung inference call
//! can never take down a receive loop.

use crate::config::{ChatConfig, UserProfile};
use crate::gate::ChatApprovalGate;
use crate::router::{ApprovalRouter, TakeResult};
use crate::sink::ChatEventSink;
use crate::transport::{drain_outbox, ApprovalButtonPress, ChatRef, ChatTransport, PressOutcome};
use amparo_agent::{Agent, AgentConfig, CaseLibrary, EventSink, FanoutSink};
use amparo_inference::InferenceProvider;
use amparo_notebook::{
    CaseRetriever, NotebookSink, SkillLogEvent, SkillSet, append_event, auto_rollup,
    check_skill_drift,
};
use amparo_policy::wire::WirePolicyEngine;
use amparo_policy::PolicyEngine;
use amparo_privacy::PrivacyPolicy;
use amparo_tools::registry::default_registry_with_policy;
use amparo_tools::{
    Memory, PathPolicy, SkillLibrary, ToolRegistry, ToolTrustTier, UseSkillTool,
};
use std::collections::HashSet;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex};

/// The toast shown to a user who presses a button on someone else's
/// pending approval — their press is ignored, the buttons stay, and the
/// requester's own press still routes.
pub const WRONG_USER_TOAST: &str = "Only the user who started the task can decide.";

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

/// Who may start tasks in chat.
pub enum Tenants {
    /// The TOML profile file (per-user workspaces, ceilings). Keyed by
    /// `"platform:user_id"`; a user with no profile is refused.
    Directory(Arc<ChatConfig>),
    /// The legacy flat `AMPARO_CHAT_ALLOWLIST` (shared workspace and
    /// ceiling for every allowed user).
    LegacyAllowlist(HashSet<String>),
}

/// Which policy engine each task gets.
pub enum PolicySource {
    /// One shared engine for every task (AllowAll / DenyAll / custom).
    Shared(Arc<dyn PolicyEngine>),
    /// A fresh engine per task, session-tagged `platform:user_id`.
    Wire {
        /// The engine's `/check` root.
        base_url: String,
        /// Optional bearer key for the engine.
        api_key: Option<String>,
    },
}

/// The chat driver: one per receive loop, the sole owner of task policy.
///
/// A message from a permitted user claims its chat (one task at a time),
/// and the task is a fresh [`amparo_agent::Agent`] built from the shared
/// parts — the same provider, with a policy engine, tool registry and
/// trust ceiling resolved per task — plus a per-task event sink and a
/// [`ChatApprovalGate`] bound to that chat. The run happens inside its own
/// `tokio::spawn`, so a panic surfaces as a `JoinError` and the chat gets
/// "The task crashed" instead of the loop going down with it.
///
/// Fail-closed: no tenant (an empty directory or allowlist) denies everyone
/// (no message ever starts a task), and the busy claim is released by a
/// drop guard, never by control flow that could be skipped.
///
/// Tenancy and policy are resolved per message, before the busy claim, so
/// a refused user never holds a chat busy. [`Tenants::Directory`] consults
/// the TOML tenant directory for a `platform:user_id` profile — the
/// profile's workspace (a subpath of the driver's root, or
/// `users/<platform>-<user_id>`) gets a fresh [`PathPolicy`] and a
/// [`default_registry_with_policy`] rooted at it, and the profile's trust
/// ceiling overrides the driver's. [`Tenants::LegacyAllowlist`] keeps the
/// M4 flat allowlist with the shared registry, root workspace and flag
/// ceiling. The policy engine is either shared across tasks
/// ([`PolicySource::Shared`]) or a fresh session-tagged
/// [`WirePolicyEngine`] per task ([`PolicySource::Wire`]) whose session id
/// is `platform:user_id` — the per-user seam M4 called out, now wired.
pub struct ChatDriver {
    /// Who may start tasks, and how their per-task parts are resolved.
    tenants: Tenants,
    /// The shared inference provider, shared by every task's agent.
    provider: Arc<dyn InferenceProvider>,
    /// How each task's policy engine is obtained.
    policy_source: PolicySource,
    /// Optional privacy policy, attached to every task's agent.
    privacy: Option<Arc<PrivacyPolicy>>,
    /// The optional lab notebook: with `--growth`, every completed or failed
    /// task is recorded as a PII-stripped, tenant-tagged run record, and the
    /// same store feeds the case library (M6b) — prior same-tenant records
    /// are retrieved into each task's self-verification prompt.
    notebook: Option<Arc<dyn Memory>>,
    /// The hot layer (M6e): when the [`ChatDriver::with_hot_layer`] seam is
    /// attached, the case library reads this store — the informative
    /// subset of the cold archive, promoted at every task start and folded
    /// daily — instead of the full notebook. Optional; without it the M6b
    /// path stands.
    hot: Option<Arc<dyn Memory>>,
    /// Where the hot layer and its rollup sidecars live; drives the
    /// promote + fold at each task start. Present only together with `hot`.
    notebook_dir: Option<PathBuf>,
    /// The legacy shared tool registry, cloned per task — ignored in
    /// directory mode, where each task gets a fresh registry rooted at its
    /// own workspace (tools are Send+Sync).
    registry: ToolRegistry,
    /// The workspace ROOT tasks operate in: per-user workspaces are
    /// subdirectories of it, and it is the legacy flat workspace.
    workspace_root: PathBuf,
    /// Outbound text and approval messages go through the platform transport.
    transport: Arc<dyn ChatTransport>,
    /// Routes button presses back to the gate that is waiting for them.
    router: Arc<ApprovalRouter>,
    /// `true` = approvals succeed without asking (deliberate unattended
    /// mode — the equivalent of `--auto-approve`).
    auto_approve: bool,
    /// The trust ceiling applied to every task's agent unless the tenant
    /// profile overrides it — tools at tiers above it are blocked outright
    /// (the `--trust-ceiling` knob of `amparo run`). Defaults to
    /// [`ToolTrustTier::SystemControl`].
    trust_ceiling: ToolTrustTier,
    /// Chat ids with a task in flight. std Mutex, and never held across an
    /// await — claim and release are synchronous.
    busy: Arc<Mutex<HashSet<String>>>,
}

/// The per-task parts resolved from the tenant directory before a task is
/// assembled: every task gets its own policy engine, tool registry and
/// trust ceiling.
struct TaskParts {
    policy: Arc<dyn PolicyEngine>,
    registry: ToolRegistry,
    trust_ceiling: ToolTrustTier,
}

impl ChatDriver {
    /// Build a driver.
    ///
    /// `tenants` decides who may start tasks ([`Tenants::Directory`] for
    /// the TOML tenant directory, [`Tenants::LegacyAllowlist`] for the flat
    /// allowlist — empty means nobody, and every message is refused).
    /// `policy_source` decides each task's policy engine
    /// ([`PolicySource::Shared`] for one shared engine,
    /// [`PolicySource::Wire`] for a fresh session-tagged engine per task).
    /// `registry` is the legacy shared registry, used only in allowlist
    /// mode. `workspace_root` is the directory per-user workspaces are
    /// scoped under (and the shared workspace in allowlist mode). The gate
    /// parts (`transport`, `router`, `auto_approve`) build one
    /// [`ChatApprovalGate`] per task, bound to that task's chat.
    pub fn new(
        tenants: Tenants,
        provider: Arc<dyn InferenceProvider>,
        policy_source: PolicySource,
        registry: ToolRegistry,
        workspace_root: PathBuf,
        transport: Arc<dyn ChatTransport>,
        router: Arc<ApprovalRouter>,
        auto_approve: bool,
    ) -> Self {
        Self {
            tenants,
            provider,
            policy_source,
            privacy: None,
            notebook: None,
            hot: None,
            notebook_dir: None,
            registry,
            workspace_root,
            transport,
            router,
            auto_approve,
            trust_ceiling: ToolTrustTier::SystemControl,
            busy: Arc::new(Mutex::new(HashSet::new())),
        }
    }

    /// Attach a privacy policy to every task this driver runs.
    pub fn with_privacy(mut self, policy: Arc<PrivacyPolicy>) -> Self {
        self.privacy = Some(policy);
        self
    }

    /// Set the trust ceiling applied to every task's agent.
    ///
    /// [`ToolTrustTier::SystemControl`] (the default) blocks nothing; a
    /// lower ceiling blocks tools above it outright, exactly like
    /// `--trust-ceiling` on `amparo run`. In directory mode this is the
    /// fallback for profiles without their own ceiling.
    pub fn with_trust_ceiling(mut self, ceiling: ToolTrustTier) -> Self {
        self.trust_ceiling = ceiling;
        self
    }

    /// Attach a lab notebook to every task this driver runs.
    ///
    /// Each task's events are fanned out to the notebook and recorded as a
    /// PII-stripped run record tagged `platform:user_id` (the same key
    /// tenancy uses), and the same store feeds the per-task case library:
    /// prior same-tenant records are retrieved into the self-verification
    /// prompt. This is the `--growth` knob of `amparo chat` — growth is
    /// write + read; the default (no notebook) records and retrieves
    /// nothing.
    pub fn with_growth(mut self, store: Arc<dyn Memory>) -> Self {
        self.notebook = Some(store);
        self
    }

    /// Attach the hot layer (M6e) — the informative subset of the lab
    /// notebook.
    ///
    /// With this seam attached, every task start promotes the cold tail
    /// into the hot store — folding it when the last fold is at least 24
    /// hours old (promoted cases exempt) — and the per-task case library
    /// reads hot instead of the full notebook. Without it the M6b path
    /// stands: retrieval over the [`ChatDriver::with_growth`] store, no
    /// rollup. The hot store is read-only by convention — only the rollup
    /// writes it.
    pub fn with_hot_layer(mut self, hot: Arc<dyn Memory>, notebook_dir: PathBuf) -> Self {
        self.hot = Some(hot);
        self.notebook_dir = Some(notebook_dir);
        self
    }

    /// Whether the user in `chat` may start tasks — the tenant-aware
    /// replacement for M4's flat `allowlisted`.
    ///
    /// Directory mode: the `platform:user_id` key must have a profile.
    /// Allowlist mode: the user id must be in the flat set.
    pub fn allows(&self, chat: &ChatRef) -> bool {
        match &self.tenants {
            Tenants::Directory(config) => {
                config.users.contains_key(&format!("{}:{}", chat.platform, chat.user_id))
            }
            Tenants::LegacyAllowlist(allowlist) => allowlist.contains(&chat.user_id),
        }
    }

    /// The workspace root tasks operate in (per-user workspaces are
    /// subdirectories of it).
    pub fn workspace(&self) -> &Path {
        &self.workspace_root
    }

    /// Resolve the per-task parts (policy, registry, ceiling) for `chat`,
    /// or `None` when the task must be refused.
    ///
    /// Called before the busy claim, so a refused user never holds a chat
    /// busy. Directory mode resolves the user's profile, per-user
    /// workspace and ceiling; allowlist mode reuses the shared parts.
    fn task_parts(&self, chat: &ChatRef) -> Option<TaskParts> {
        match &self.tenants {
            Tenants::Directory(config) => {
                let key = format!("{}:{}", chat.platform, chat.user_id);
                let profile = config.users.get(&key)?;
                let workspace = self.workspace_for(chat, profile)?;
                let path_policy = Arc::new(PathPolicy::from_root(workspace));
                Some(TaskParts {
                    policy: self.task_policy(chat),
                    registry: default_registry_with_policy(path_policy),
                    trust_ceiling: profile.trust_ceiling.unwrap_or(self.trust_ceiling),
                })
            }
            Tenants::LegacyAllowlist(_) => Some(TaskParts {
                policy: self.task_policy(chat),
                registry: self.registry.clone(),
                trust_ceiling: self.trust_ceiling,
            }),
        }
    }

    /// The policy engine for one task: the shared engine, or a fresh
    /// [`WirePolicyEngine`] session-tagged `platform:user_id` (correlates
    /// engine-side audit rows with the chat user).
    fn task_policy(&self, chat: &ChatRef) -> Arc<dyn PolicyEngine> {
        match &self.policy_source {
            PolicySource::Shared(engine) => Arc::clone(engine),
            PolicySource::Wire { base_url, api_key } => Arc::new(
                WirePolicyEngine::new(base_url.clone(), api_key.clone())
                    .with_session_id(format!("{}:{}", chat.platform, chat.user_id)),
            ),
        }
    }

    /// The workspace directory for one directory-mode tenant: the profile's
    /// subpath under the root, or `users/<platform>-<user_id>`.
    ///
    /// Defense-in-depth: the config loader already rejects absolute and
    /// `..`-containing workspace paths, but the user id itself is
    /// interpolated into the default path — a hostile id (`../../x`) must
    /// not escape the root. The joined path's portion below the root is
    /// re-checked for [`Component::ParentDir`]/[`Component::RootDir`]; a
    /// hit refuses the task. Creation is best-effort — the tools surface
    /// real failures.
    fn workspace_for(&self, chat: &ChatRef, profile: &UserProfile) -> Option<PathBuf> {
        let joined = profile
            .workspace
            .as_ref()
            .map(|w| self.workspace_root.join(w))
            .unwrap_or_else(|| {
                self.workspace_root
                    .join("users")
                    .join(format!("{}-{}", chat.platform, chat.user_id))
            });
        // The root's own leading RootDir is expected for absolute roots —
        // only the portion the tenant contributed is hostile territory.
        let tail = joined.strip_prefix(&self.workspace_root).unwrap_or(joined.as_path());
        if tail.components().any(|c| matches!(c, Component::ParentDir | Component::RootDir)) {
            return None;
        }
        let _ = std::fs::create_dir_all(&joined);
        Some(joined)
    }

    /// Handle one inbound message.
    ///
    /// A user without a tenant → a polite refusal. Busy → a "still running"
    /// reply. Otherwise the per-task parts are resolved, the chat is
    /// claimed and a per-task future is spawned that builds a fresh agent
    /// (resolved parts, per-chat approval gate, per-task event sink), runs
    /// it inside its own spawn for panic containment, and delivers the
    /// final answer with a direct awaited `send_text` — bypassing the
    /// event sink so it can never be lost.
    pub async fn on_message(&self, chat: ChatRef, text: String) {
        if !self.allows(&chat) {
            let _ = self
                .transport
                .send_text(&chat, "This chat is not authorized to use this agent.")
                .await;
            return;
        }

        // Resolve the per-task parts before claiming the chat — a refused
        // user (e.g. a hostile tenant workspace) must not hold the chat
        // busy. The refusal string is byte-identical to the M4 one.
        let Some(parts) = self.task_parts(&chat) else {
            let _ = self
                .transport
                .send_text(&chat, "This chat is not authorized to use this agent.")
                .await;
            return;
        };

        // The claim-or-busy decision is made inside a block so the
        // MutexGuard is dropped before any await — the future must stay
        // Send for the transports' receive loops.
        let busy_reply = {
            let mut busy = self.busy.lock().unwrap();
            if busy.contains(&chat.chat_id) {
                true
            } else {
                busy.insert(chat.chat_id.clone());
                false
            }
        };
        if busy_reply {
            let _ = self
                .transport
                .send_text(&chat, "Another task is still running — wait for it to finish.")
                .await;
            return;
        }

        let TaskParts { policy, mut registry, trust_ceiling } = parts;
        let provider = Arc::clone(&self.provider);
        let privacy = self.privacy.clone();
        let transport = Arc::clone(&self.transport);
        let router = Arc::clone(&self.router);
        let busy = Arc::clone(&self.busy);
        let auto_approve = self.auto_approve;
        // The notebook's tenant tag is computed fresh per message — the
        // same `platform:user_id` key tenancy uses.
        let tenant_key = format!("{}:{}", chat.platform, chat.user_id);
        let notebook = self.notebook.clone();
        let hot = self.hot.clone();
        let notebook_dir = self.notebook_dir.clone();
        let workspace_root = self.workspace_root.clone();

        tokio::spawn(async move {
            // The claim is the busy-map entry; dropping it (however this
            // task ends) releases the chat.
            let _claim = ChatClaim { busy, chat_id: chat.chat_id.clone() };

            // Progress events flow through a best-effort outbox; the final
            // answer below bypasses it. With a notebook attached, the same
            // events also fan out to it (growth is observational — a record
            // write can never fail or block the task).
            let (chat_sink, rx) = ChatEventSink::channel();
            // The notebook sink is named so the success branch can await
            // its final write before the answer goes out — that closes
            // the back-to-back task window where the next task's
            // promotion could miss this task's record (M6e).
            let notebook_sink: Option<Arc<NotebookSink>> = notebook.as_ref().map(|store| {
                Arc::new(NotebookSink::new(Arc::clone(store), tenant_key.clone()))
            });
            let sink: Arc<dyn EventSink> = match &notebook_sink {
                Some(nb) => Arc::new(FanoutSink::new(vec![
                    chat_sink,
                    Arc::clone(nb) as Arc<dyn EventSink>,
                ])),
                None => chat_sink,
            };
            // The same store feeds the case library (M6b); with the hot
            // layer attached (M6e) the hot store feeds it instead. Prior
            // runs by this tenant are retrieved into the self-verification
            // prompt — never into the action loop.
            // Gated skills (M6c + M6d): with a notebook attached
            // (--growth), this tenant's adopted skills register `use_skill`
            // on the task's own registry copy. The load is tenant-filtered
            // exactly like M6b retrieval, so another tenant's skills are
            // invisible (I2). Startup drift re-check (M6d, drift only — no
            // records parse per task): a skill whose step plan this task's
            // policy/ceiling/registry would block retires before
            // registration. Growth is observational — a write failure
            // warns and continues, never failing the task.
            let mut skills: Option<Arc<dyn SkillLibrary>> = None;
            if notebook.is_some() {
                let skills_path = workspace_root.join(".amparo/skills/adopted.jsonl");
                let skill_set = SkillSet::load(&skills_path, &tenant_key);
                let adopted_names = skill_set.names();
                if !adopted_names.is_empty() {
                    let now = chrono::Utc::now().to_rfc3339();
                    for name in &adopted_names {
                        let Some(spec) = skill_set.get(name) else { continue; };
                        if let Some(reason) =
                            check_skill_drift(&spec, &registry, trust_ceiling, policy.as_ref())
                                .await
                        {
                            if let Err(e) = append_event(
                                &skills_path,
                                &SkillLogEvent::retire(&tenant_key, name, &now, reason.clone()),
                            ) {
                                eprintln!("[growth] retire write failed: {e}");
                            }
                            eprintln!("[growth] skill {name} retired: {reason}");
                        }
                    }
                    let survivors = SkillSet::load(&skills_path, &tenant_key);
                    if !survivors.names().is_empty() {
                        let library: Arc<dyn SkillLibrary> = Arc::new(survivors);
                        registry.register(Arc::new(UseSkillTool::new(Arc::clone(&library))));
                        skills = Some(library);
                    }
                }
            }
            // Rollup and archival (M6e): with the hot layer attached,
            // promote the cold tail into hot — folding it daily — before
            // retrieval, so the case library reads this workspace's hot
            // copy too. Observational: a failure warns and never fails
            // the task.
            if let Some(nb_dir) = &notebook_dir {
                match auto_rollup(nb_dir, chrono::Utc::now()) {
                    Ok(Some(report)) if report.promoted > 0 => {
                        eprintln!(
                            "[growth] notebook: promoted {} tail record(s) to the hot layer",
                            report.promoted
                        );
                    }
                    Ok(_) => {}
                    Err(e) => eprintln!("[growth] notebook rollup failed: {e}"),
                }
            }
            // The case library reads the hot layer when it is attached
            // (M6e); otherwise the full notebook does (M6b). Either way,
            // prior runs by this tenant inform self-verification — never
            // the action loop.
            let case_library: Option<Arc<dyn CaseLibrary>> = hot
                .as_ref()
                .map(|store| {
                    Arc::new(CaseRetriever::new(Arc::clone(store), tenant_key.clone()))
                        as Arc<dyn CaseLibrary>
                })
                .or_else(|| {
                    notebook.as_ref().map(|store| {
                        Arc::new(CaseRetriever::new(Arc::clone(store), tenant_key))
                            as Arc<dyn CaseLibrary>
                    })
                });
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
            if let Some(library) = case_library {
                agent = agent.with_case_library(library);
            }
            if let Some(library) = skills {
                agent = agent.with_skills(library);
            }
            // The flags' trust ceiling reaches the per-task agent through
            // the shared agent config (the `amparo run` equivalent).
            agent = agent.with_config(AgentConfig { trust_ceiling, ..AgentConfig::default() });

            // Run inside its own spawn so a panic becomes a JoinError
            // instead of taking down this task — and the receive loop.
            let run = tokio::spawn(async move { agent.run(text).await });
            match run.await {
                Ok(report) => {
                    // Flush the pending record before the answer lands, so
                    // the next task in this chat always promotes this one
                    // (M6e — the back-to-back soft-miss window).
                    if let Some(nb) = &notebook_sink {
                        nb.flush().await;
                    }
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

    /// Handle one inline-button press.
    ///
    /// Returns how the press was resolved: [`PressOutcome::Routed`] when the
    /// decision reached a waiting gate, [`PressOutcome::AlreadyDecided`]
    /// when no entry exists (double press, timeout, or never registered),
    /// and [`PressOutcome::WrongUser`] when the presser is not the user who
    /// started the task — the entry stays for the requester's own press.
    pub async fn on_approval(&self, press: ApprovalButtonPress) -> PressOutcome {
        match self.router.take(&press.chat_id, &press.approval_id, &press.user_id).await {
            TakeResult::Routed(tx) => {
                // The gate may have timed out and dropped its receiver just
                // now — the press is still consumed (already decided), never
                // replayed.
                let _ = tx.send(press.approved);
                PressOutcome::Routed
            }
            TakeResult::AlreadyDecided => PressOutcome::AlreadyDecided,
            TakeResult::WrongUser => PressOutcome::WrongUser,
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
    use amparo_notebook::{HOT_FILE, JsonlStore, SkillLogEvent, append_event};
    use amparo_policy::AllowAllPolicyEngine;
    use amparo_tools::{InMemoryStore, SkillOrigin, SkillSpec, SkillStep, ToolTrustTier};
    use common::{
        done_frame, registry_with_echo, tool_call_frame, turn_text, turn_tool_call,
        wait_for_text, wait_until, wait_until_async, MockTransport, StubProvider,
    };
    use std::collections::BTreeMap;

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
            Tenants::LegacyAllowlist(allowlist),
            provider,
            PolicySource::Shared(Arc::new(AllowAllPolicyEngine)),
            registry_with_echo(echo_tier),
            PathBuf::from("/tmp/amparo-chat-test"),
            transport,
            Arc::new(ApprovalRouter::new()),
            auto_approve,
        )
    }

    /// A directory-mode driver: the tenant directory is built by hand (no
    /// file I/O); the shared registry is unused because directory mode
    /// builds a fresh per-user registry.
    fn driver_directory(
        users: BTreeMap<String, UserProfile>,
        transport: Arc<MockTransport>,
        provider: Arc<StubProvider>,
        auto_approve: bool,
        root: PathBuf,
    ) -> ChatDriver {
        ChatDriver::new(
            Tenants::Directory(Arc::new(ChatConfig { users })),
            provider,
            PolicySource::Shared(Arc::new(AllowAllPolicyEngine)),
            ToolRegistry::new(),
            root,
            transport,
            Arc::new(ApprovalRouter::new()),
            auto_approve,
        )
    }

    fn profile() -> UserProfile {
        UserProfile { trust_ceiling: None, workspace: None }
    }

    /// A chat for `user_id` in its own chat (so concurrent tasks don't
    /// collide on the busy claim).
    fn chat_for(user_id: &str) -> ChatRef {
        ChatRef {
            platform: "mock",
            chat_id: format!("chat_{user_id}"),
            user_id: user_id.into(),
        }
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
            user_id: "user_1".into(),
        };
        assert_eq!(
            driver.on_approval(press).await,
            PressOutcome::Routed,
            "the press reached the waiting gate"
        );
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
            user_id: "user_1".into(),
        };
        assert_eq!(driver.on_approval(press).await, PressOutcome::AlreadyDecided);
    }

    #[tokio::test]
    async fn wrong_user_press_is_reported_and_requester_still_decides() {
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

        // Another user presses Approve: refused, and the entry is kept.
        let wrong = ApprovalButtonPress {
            chat_id: "chat_1".into(),
            approval_id: "call_1".into(),
            approved: true,
            user_id: "user_2".into(),
        };
        assert_eq!(driver.on_approval(wrong).await, PressOutcome::WrongUser);

        // The requester's own press still routes and the task finishes.
        let requester = ApprovalButtonPress {
            chat_id: "chat_1".into(),
            approval_id: "call_1".into(),
            approved: true,
            user_id: "user_1".into(),
        };
        assert_eq!(driver.on_approval(requester).await, PressOutcome::Routed);
        wait_for_text(&transport, "Done.").await;
    }

    #[tokio::test]
    async fn provider_panic_sends_crash_notice() {
        let transport = MockTransport::new();
        let driver = driver(transport.clone(), StubProvider::panicking(), true);
        driver.on_message(chat(), "cause a panic".into()).await;
        wait_for_text(&transport, "The task crashed").await;
    }

    #[tokio::test]
    async fn growth_records_a_tenant_tagged_stripped_record() {
        let transport = MockTransport::new();
        let provider = StubProvider::new(vec![turn_text("Done.")]);
        let store = Arc::new(InMemoryStore::new());
        let driver = driver(transport.clone(), provider, true).with_growth(store.clone());
        driver
            .on_message(chat(), "remember my email user@example.com please".into())
            .await;
        wait_for_text(&transport, "Done.").await;
        // The record write lands on a spawned task — poll the store.
        wait_until_async(|| async {
            store
                .search("mock:user_1", 10)
                .await
                .iter()
                .any(|e| e.content.contains(r#""tenant_id":"mock:user_1""#))
        })
        .await;
        let entries = store.search("mock:user_1", 10).await;
        let record = entries
            .iter()
            .find(|e| e.content.contains(r#""tenant_id":"mock:user_1""#))
            .expect("a tenant-tagged record landed");
        assert!(!record.content.contains("user@example.com"), "raw email stripped");
        assert!(record.content.contains("[EMAIL_1]"), "stripped placeholder kept");
    }

    #[tokio::test]
    async fn growth_retrieves_prior_same_user_cases_into_verification() {
        let transport = MockTransport::new();
        let provider = StubProvider::new(vec![turn_text("Done.")]);
        let store = Arc::new(InMemoryStore::new());
        let driver = driver(transport.clone(), provider.clone(), true).with_growth(store.clone());

        // Task 1 seeds the notebook with a VERIFIED case for this user.
        driver.on_message(chat(), "deploy the staging site".into()).await;
        wait_for_text(&transport, "Done.").await;
        wait_until_async(|| async {
            store
                .search("mock:user_1", 10)
                .await
                .iter()
                .any(|e| e.content.contains(r#""tenant_id":"mock:user_1""#))
        })
        .await;

        // Task 2: its verification prompt must carry the prior case — the
        // same user, retrieved read-only.
        driver.on_message(chat(), "deploy the staging site".into()).await;
        wait_until(|| provider.recorded_complete_prompts().len() == 2).await;

        let prompts = provider.recorded_complete_prompts();
        assert_eq!(prompts.len(), 2, "one verification per task: {prompts:?}");
        assert!(
            !prompts[0].contains("Prior cases"),
            "an empty notebook leaves the first verification unchanged: {}",
            prompts[0]
        );
        assert!(
            prompts[1].contains("Prior cases in this tenant resembling the current task:"),
            "the second verification carries the evidence section: {}",
            prompts[1]
        );
        assert!(
            prompts[1].contains("deploy the staging site"),
            "the evidence names the prior task: {}",
            prompts[1]
        );
    }

    #[tokio::test]
    async fn growth_hot_layer_promotes_tail_and_retrieves_from_hot() {
        // The M6e seam: a temp root's cold archive plus its hot layer.
        // Task 1 writes the cold record (flushed before its answer);
        // task 2's start promotes the tail into hot and its verification
        // reads the promoted case from hot.
        let root =
            std::env::temp_dir().join(format!("amparo-chat-hot-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let transport = MockTransport::new();
        let provider = StubProvider::new(vec![turn_text("Done.")]);
        let nb_dir = root.join(".amparo/notebook");
        let cold = Arc::new(JsonlStore::open(nb_dir.join("records.jsonl")).expect("cold store"));
        let hot = Arc::new(JsonlStore::open(nb_dir.join(HOT_FILE)).expect("hot store"));
        let driver = driver(transport.clone(), provider.clone(), true)
            .with_growth(cold)
            .with_hot_layer(hot, nb_dir);

        // Task 1 seeds the cold archive.
        driver.on_message(chat(), "deploy the staging site".into()).await;
        wait_for_text(&transport, "Done.").await;
        let cold_path = root.join(".amparo/notebook/records.jsonl");
        wait_until(|| {
            std::fs::read_to_string(&cold_path)
                .map(|t| t.lines().count() == 1)
                .unwrap_or(false)
        })
        .await;

        // Task 2: the startup rollup promotes task 1's tail into hot, and
        // the second verification carries the case from the hot layer.
        driver.on_message(chat_for("user_1"), "deploy the staging site".into()).await;
        wait_until(|| provider.recorded_complete_prompts().len() == 2).await;

        let hot_path = root.join(".amparo/notebook/hot.jsonl");
        let hot_rows = std::fs::read_to_string(&hot_path).expect("hot layer written");
        assert_eq!(hot_rows.lines().count(), 1, "one hot row: {hot_rows}");
        let id_of = |text: &str| -> String {
            serde_json::from_str::<serde_json::Value>(text.lines().next().expect("one row"))
                .expect("entry JSON")["id"]
                .as_str()
                .expect("entry id")
                .to_string()
        };
        let cold_rows = std::fs::read_to_string(&cold_path).expect("cold archive");
        assert_eq!(id_of(&hot_rows), id_of(&cold_rows), "the hot row keeps the cold id");

        let prompts = provider.recorded_complete_prompts();
        assert!(
            !prompts[0].contains("Prior cases"),
            "an empty hot layer leaves the first verification unchanged: {}",
            prompts[0]
        );
        assert!(
            prompts[1].contains("Prior cases in this tenant resembling the current task:"),
            "the second verification reads the promoted case from hot: {}",
            prompts[1]
        );
        assert!(
            prompts[1].contains("deploy the staging site"),
            "the evidence names the prior task: {}",
            prompts[1]
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A fresh workspace root per test (skills land under `<root>/.amparo`).
    fn temp_skills_root(tag: &str) -> PathBuf {
        let root =
            std::env::temp_dir().join(format!("amparo-chat-skills-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        root
    }

    /// Seeds `demo-skill` (one `run_command` step) adopted by `mock:user_1`
    /// only, then builds a two-tenant directory-mode driver with growth.
    fn seeded_skill_driver(
        root: &Path,
        transport: Arc<MockTransport>,
        provider: Arc<StubProvider>,
    ) -> (ChatDriver, Arc<InMemoryStore>) {
        let mut users = BTreeMap::new();
        users.insert("mock:user_1".to_string(), profile());
        users.insert("mock:user_2".to_string(), profile());
        let spec = SkillSpec {
            name: "demo-skill".into(),
            description: "one command step".into(),
            preconditions: vec![],
            steps: vec![SkillStep {
                tool: "run_command".into(),
                arguments: serde_json::json!({"command": "echo from-skill"}),
            }],
            expected_outcome: "the command runs".into(),
            origin: SkillOrigin::Operator,
            source_run_ids: vec![],
            adopted_at: None,
        };
        append_event(
            &root.join(".amparo/skills/adopted.jsonl"),
            &SkillLogEvent::adopt(
                "mock:user_1",
                "demo-skill",
                spec,
                "2026-08-29T00:00:00Z",
            ),
        )
        .expect("the seed adoption writes");
        let store = Arc::new(InMemoryStore::new());
        let driver =
            driver_directory(users, transport, provider, true, root.to_path_buf())
                .with_growth(store.clone());
        (driver, store)
    }

    /// A tenant's run record whose tool_calls include `tool` (polled — the
    /// record write lands on the spawned task).
    async fn tenant_record(store: &InMemoryStore, tenant: &str, tool: &str) -> String {
        wait_until_async(|| async {
            store
                .search(tenant, 10)
                .await
                .iter()
                .any(|e| e.content.contains(&format!(r#""tool_name":"{tool}""#)))
        })
        .await;
        store
            .search(tenant, 10)
            .await
            .into_iter()
            .map(|e| e.content)
            .find(|c| c.contains(&format!(r#""tool_name":"{tool}""#)))
            .expect("the record carries the call")
    }

    #[tokio::test]
    async fn growth_skills_execute_for_the_adopting_tenant() {
        let root = temp_skills_root("adopting");
        let transport = MockTransport::new();
        let provider = StubProvider::new(vec![
            turn_tool_call("call_1", "use_skill", r#"{"skill_name":"demo-skill"}"#),
            turn_text("Skill done."),
        ]);
        let (driver, store) = seeded_skill_driver(&root, transport.clone(), provider);
        driver.on_message(chat(), "use the demo skill".into()).await;
        wait_for_text(&transport, "Skill done.").await;
        let record = tenant_record(&store, "mock:user_1", "use_skill").await;
        assert!(
            record.contains(r#""decision":"allowed""#),
            "use_skill passed the gate: {record}"
        );
        assert!(
            record.contains(r#""tool_name":"run_command""#),
            "the step ran through its own gate: {record}"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn growth_skills_are_invisible_to_other_tenants() {
        let root = temp_skills_root("isolation");
        let transport = MockTransport::new();
        let provider = StubProvider::new(vec![
            turn_tool_call("call_1", "use_skill", r#"{"skill_name":"demo-skill"}"#),
            turn_text("Done."),
        ]);
        let (driver, store) = seeded_skill_driver(&root, transport.clone(), provider);
        driver.on_message(chat_for("user_2"), "use the demo skill".into()).await;
        wait_for_text(&transport, "Done.").await;
        let record = tenant_record(&store, "mock:user_2", "use_skill").await;
        assert!(
            record.contains(r#""decision":"unknown_tool""#),
            "use_skill is unknown without an adoption: {record}"
        );
        assert!(
            !record.contains(r#""tool_name":"run_command""#),
            "no step expansion for the other tenant: {record}"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn growth_skill_drift_retires_before_registration() {
        let root = temp_skills_root("drift");
        let transport = MockTransport::new();
        let provider = StubProvider::new(vec![
            turn_tool_call("call_1", "use_skill", r#"{"skill_name":"demo-skill"}"#),
            turn_text("Done."),
        ]);
        // The step tool `echo` is not in the directory-mode task registry
        // (default_registry_with_policy), so the startup drift check
        // retires the skill before use_skill is ever registered.
        let mut users = BTreeMap::new();
        users.insert("mock:user_1".to_string(), profile());
        let spec = SkillSpec {
            name: "demo-skill".into(),
            description: "one echo step".into(),
            preconditions: vec![],
            steps: vec![SkillStep {
                tool: "echo".into(),
                arguments: serde_json::json!({"message": "from-skill"}),
            }],
            expected_outcome: "the echo runs".into(),
            origin: SkillOrigin::Operator,
            source_run_ids: vec![],
            adopted_at: None,
        };
        append_event(
            &root.join(".amparo/skills/adopted.jsonl"),
            &SkillLogEvent::adopt(
                "mock:user_1",
                "demo-skill",
                spec,
                "2026-08-29T00:00:00Z",
            ),
        )
        .expect("the seed adoption writes");
        let store = Arc::new(InMemoryStore::new());
        let driver = driver_directory(users, transport.clone(), provider, true, root.clone())
            .with_growth(store.clone());

        driver.on_message(chat(), "use the demo skill".into()).await;
        wait_for_text(&transport, "Done.").await;

        // The scripted use_skill hit an empty registry: unknown_tool.
        let record = tenant_record(&store, "mock:user_1", "use_skill").await;
        assert!(
            record.contains(r#""decision":"unknown_tool""#),
            "the drifted skill never registered: {record}"
        );

        // The startup check wrote the retire event with an honest reason.
        let log = std::fs::read_to_string(root.join(".amparo/skills/adopted.jsonl"))
            .expect("the audit log exists");
        let row: serde_json::Value = serde_json::from_str(
            log.lines().last().expect("the retire row"),
        )
        .expect("the retire row is JSON");
        assert_eq!(row["event"], "retire");
        assert!(
            row["reason"]
                .as_str()
                .expect("the reason is a string")
                .contains("policy drift"),
            "the reason names drift: {row}"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn directory_mode_refuses_unlisted_user() {
        let transport = MockTransport::new();
        let provider = StubProvider::new(vec![]);
        let mut users = BTreeMap::new();
        // Only some other user is listed — user_1 is not a tenant.
        users.insert("mock:someone_else".to_string(), profile());
        let driver = driver_directory(
            users,
            transport.clone(),
            provider.clone(),
            true,
            PathBuf::from("/tmp/amparo-chat-test-dir"),
        );
        driver.on_message(chat(), "do something".into()).await;
        let line = wait_for_text(&transport, "not authorized").await;
        assert!(line.starts_with("This chat is not authorized"), "{line}");
        assert!(
            provider.recorded_requests().is_empty(),
            "no LLM request for a refused user"
        );
    }

    #[tokio::test]
    async fn directory_mode_allows_listed_user() {
        let transport = MockTransport::new();
        let provider = StubProvider::new(vec![turn_text("The answer is 42.")]);
        let mut users = BTreeMap::new();
        users.insert("mock:user_1".to_string(), profile());
        let driver = driver_directory(
            users,
            transport.clone(),
            provider.clone(),
            true,
            PathBuf::from("/tmp/amparo-chat-test-dir"),
        );
        driver.on_message(chat(), "what is the answer?".into()).await;
        wait_for_text(&transport, "The answer is 42.").await;
        assert_eq!(provider.recorded_requests().len(), 1, "one LLM request");
    }

    #[tokio::test]
    async fn directory_mode_per_user_ceiling_blocks() {
        let transport = MockTransport::new();
        let provider = StubProvider::new(vec![
            turn_tool_call("call_1", "run_command", r#"{"command":"echo hi"}"#),
            turn_text("Done."),
        ]);
        let mut users = BTreeMap::new();
        // The profile ceiling (Observational) blocks run_command
        // (ExternalEffector) even though the driver's flag ceiling is the
        // permissive default.
        users.insert(
            "mock:user_1".to_string(),
            UserProfile { trust_ceiling: Some(ToolTrustTier::Observational), workspace: None },
        );
        let driver = driver_directory(
            users,
            transport.clone(),
            provider.clone(),
            true,
            PathBuf::from("/tmp/amparo-chat-test-dir"),
        );
        driver.on_message(chat(), "run a thing".into()).await;
        wait_for_text(&transport, "Done.").await;
        assert!(transport.approvals().is_empty(), "no approval for a trust-blocked call");
        let requests = provider.recorded_requests();
        assert!(
            requests.iter().any(|r| r.contains("tool blocked by trust ceiling")),
            "the blocked call was answered: {requests:?}"
        );
    }

    #[tokio::test]
    async fn directory_mode_ceiling_falls_back_to_flag() {
        let transport = MockTransport::new();
        let provider = StubProvider::new(vec![
            turn_tool_call("call_1", "run_command", r#"{"command":"echo hi"}"#),
            turn_text("Done."),
        ]);
        let mut users = BTreeMap::new();
        // No profile ceiling — the driver's flag ceiling applies.
        users.insert("mock:user_1".to_string(), profile());
        let driver = driver_directory(
            users,
            transport.clone(),
            provider.clone(),
            true,
            PathBuf::from("/tmp/amparo-chat-test-dir"),
        )
        .with_trust_ceiling(ToolTrustTier::Observational);
        driver.on_message(chat(), "run a thing".into()).await;
        wait_for_text(&transport, "Done.").await;
        assert!(transport.approvals().is_empty(), "no approval for a trust-blocked call");
        let requests = provider.recorded_requests();
        assert!(
            requests.iter().any(|r| r.contains("tool blocked by trust ceiling")),
            "the flag ceiling blocked the call: {requests:?}"
        );
    }

    #[tokio::test]
    async fn directory_mode_workspaces_are_distinct() {
        let root = std::env::temp_dir().join(format!(
            "amparo-chat-driver-ws-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        let transport = MockTransport::new();
        // One tool call per turn (tool_call_frame hardcodes index 0, and
        // the agent's SSE accumulator concatenates by index — two calls in
        // one frame would merge). The messages are driven sequentially, so
        // the shared script queue cannot interleave between the tasks.
        let provider = StubProvider::new(vec![
            // user_a (default workspace): write, then read back.
            vec![tool_call_frame("call_1", "write_file", r#"{"path":"notes.txt","content":"hello from user_a"}"#), done_frame()],
            vec![tool_call_frame("call_2", "read_file", r#"{"path":"notes.txt"}"#), done_frame()],
            turn_text("A done."),
            // user_b (workspace = "team-b"): read its own notes.txt — the
            // file lives in user_a's workspace, not here.
            vec![tool_call_frame("call_3", "read_file", r#"{"path":"notes.txt"}"#), done_frame()],
            turn_text("B done."),
        ]);
        let mut users = BTreeMap::new();
        users.insert("mock:user_a".to_string(), profile());
        users.insert(
            "mock:user_b".to_string(),
            UserProfile { trust_ceiling: None, workspace: Some(PathBuf::from("team-b")) },
        );
        let driver = driver_directory(users, transport.clone(), provider.clone(), true, root.clone());

        driver.on_message(chat_for("user_a"), "store a note".into()).await;
        wait_for_text(&transport, "A done.").await;
        driver.on_message(chat_for("user_b"), "read the note".into()).await;
        wait_for_text(&transport, "B done.").await;

        let requests = provider.recorded_requests();
        // user_a's read result returned the 17 bytes it wrote (the tool
        // message nests the result JSON, so the quotes are escaped).
        assert!(
            requests.iter().any(|r| r.contains("\\\"size_bytes\\\":17")),
            "user_a read its own file back: {requests:?}"
        );
        // user_b's read under its own root fails — the file is not there.
        assert!(
            requests.iter().any(|r| r.contains("No such file")),
            "user_b's read under its own root fails: {requests:?}"
        );
        assert!(
            root.join("users/mock-user_a/notes.txt").exists(),
            "user_a's file landed in its default workspace"
        );
        assert!(
            !root.join("team-b/notes.txt").exists(),
            "user_b's root never saw user_a's file"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Reads one full HTTP request body (head + content-length bytes).
    async fn read_http_body(
        sock: &mut tokio::net::TcpStream,
    ) -> std::io::Result<String> {
        use tokio::io::AsyncReadExt;
        let mut buf: Vec<u8> = Vec::new();
        let mut chunk = [0u8; 4096];
        let body_len = loop {
            let n = sock.read(&mut chunk).await?;
            if n == 0 {
                return Ok(String::from_utf8_lossy(&buf).to_string());
            }
            buf.extend_from_slice(&chunk[..n]);
            if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                let head = String::from_utf8_lossy(&buf[..pos]);
                let mut len = 0usize;
                for line in head.lines() {
                    if let Some(v) = line.to_lowercase().strip_prefix("content-length:") {
                        len = v.trim().parse().unwrap_or(0);
                    }
                }
                if buf.len() >= pos + 4 + len {
                    break len;
                }
            }
        };
        let head_end = buf.windows(4).position(|w| w == b"\r\n\r\n").unwrap() + 4;
        Ok(String::from_utf8_lossy(&buf[head_end..head_end + body_len]).to_string())
    }

    #[tokio::test]
    async fn wire_policy_session_id_is_per_user() {
        use tokio::io::AsyncWriteExt;
        use tokio::net::TcpListener;

        // A mock policy engine: answer every POST /check with allow, and
        // capture the request bodies. The policy gate runs for every tool
        // call, so one read_file per user means two checks.
        let captured = Arc::new(Mutex::new(Vec::new()));
        let bodies = Arc::clone(&captured);
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            for _ in 0..2 {
                let (mut sock, _) = listener.accept().await.unwrap();
                let body = read_http_body(&mut sock).await.unwrap_or_default();
                bodies.lock().unwrap().push(body);
                let resp = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\
                     connection: close\r\n\r\n{}",
                    "{\"verdict\":\"allow\"}".len(),
                    "{\"verdict\":\"allow\"}"
                );
                let _ = sock.write_all(resp.as_bytes()).await;
            }
        });

        let transport = MockTransport::new();
        // The messages are driven sequentially (each task to its final
        // answer first), so the shared script queue cannot interleave
        // between the tasks: user_a pops scripts 1-2, user_b pops 3-4.
        let provider = StubProvider::new(vec![
            vec![tool_call_frame("call_1", "read_file", r#"{"path":"notes.txt"}"#), done_frame()],
            turn_text("A done."),
            vec![tool_call_frame("call_2", "read_file", r#"{"path":"notes.txt"}"#), done_frame()],
            turn_text("B done."),
        ]);
        let mut users = BTreeMap::new();
        users.insert("mock:user_a".to_string(), profile());
        users.insert("mock:user_b".to_string(), profile());
        let driver = ChatDriver::new(
            Tenants::Directory(Arc::new(ChatConfig { users })),
            provider,
            PolicySource::Wire {
                base_url: format!("http://{addr}"),
                api_key: Some("test-key".to_string()),
            },
            ToolRegistry::new(),
            PathBuf::from("/tmp/amparo-chat-test-dir"),
            transport.clone(),
            Arc::new(ApprovalRouter::new()),
            true,
        );

        driver.on_message(chat_for("user_a"), "read a note".into()).await;
        wait_for_text(&transport, "A done.").await;
        driver.on_message(chat_for("user_b"), "read a note".into()).await;
        wait_for_text(&transport, "B done.").await;
        server.await.expect("mock policy engine served both checks");

        let bodies = captured.lock().unwrap().clone();
        assert_eq!(bodies.len(), 2, "one check per user: {bodies:?}");
        assert!(
            bodies.iter().any(|b| b.contains("\"session_id\":\"mock:user_a\"")),
            "user_a's check carries its session: {bodies:?}"
        );
        assert!(
            bodies.iter().any(|b| b.contains("\"session_id\":\"mock:user_b\"")),
            "user_b's check carries its session: {bodies:?}"
        );
    }
}
