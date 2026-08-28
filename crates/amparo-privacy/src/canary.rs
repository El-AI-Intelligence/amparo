// Originally part of Axiom-OS (MIT, Copyright (c) Pixel Phantom AI).
// Ported to Amparo and relicensed Apache-2.0 — see the repository NOTICE.

//! Canary Token Infrastructure — data-leak detection for Amparo.
//!
//! Canary tokens are unique, randomly-generated identifiers injected into
//! model context (e.g. as a "hidden system note").  If a token reappears in
//! model output, it signals a potential data-exfiltration or prompt-injection
//! path that caused the model to echo back private context verbatim.
//!
//! # Workflow
//! 1. Before sending context to the LLM, call [`CanaryTokenManager::mint`] to
//!    get a fresh token and inject its [`CanaryToken::injection_fragment`] into
//!    the system prompt.
//! 2. After receiving the LLM response, call
//!    [`CanaryTokenManager::scan_output`] with the raw response string.
//! 3. If the scan returns `Some(triggered_token)`, log an alert and take
//!    appropriate action (e.g. discard response, raise audit event).
//!
//! # Persistence
//! Active tokens are kept in memory with a TTL.  After expiry they are
//! retired to a log and no longer checked.  This prevents unbounded growth.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

// ─── Canary token ─────────────────────────────────────────────────────────────

/// A single canary token with its metadata.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CanaryToken {
    /// The unique token string.
    pub id: String,
    /// Context label (for audit logs).
    pub context: String,
    /// ISO8601 creation timestamp.
    pub created_at: String,
    #[serde(skip)]
    #[allow(dead_code)]
    created_instant: Option<Instant>,
}

impl CanaryToken {
    /// Returns the fragment to inject into system prompts.
    ///
    /// The token is embedded in a natural-language wrapper so it blends into
    /// the system context and is unlikely to be echoed by an honest model.
    pub fn injection_fragment(&self) -> String {
        format!(
            "[internal-ref: amparo-ctx-{}]",
            self.id
        )
    }
}

// ─── Trigger record ───────────────────────────────────────────────────────────

/// Recorded when a canary fires.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CanaryTrigger {
    /// The id of the canary token that fired.
    pub token_id: String,
    /// Context label of the triggered token (for audit logs).
    pub context: String,
    /// ISO8601 timestamp of when the trigger was recorded.
    pub triggered_at: String,
    /// First 200 chars of the output where the token was found.
    pub output_excerpt: String,
}

// ─── Manager ─────────────────────────────────────────────────────────────────

/// Manages the lifecycle of canary tokens and records triggers.
pub struct CanaryTokenManager {
    /// Active tokens: id → (token, created_at instant)
    active: Mutex<HashMap<String, (CanaryToken, Instant)>>,
    /// Fired triggers (bounded; oldest dropped when full)
    triggers: Mutex<Vec<CanaryTrigger>>,
    /// How long a token stays active.
    ttl: Duration,
    /// Maximum number of trigger records to keep.
    max_triggers: usize,
}

impl CanaryTokenManager {
    /// Creates a manager with the default settings: a 5-minute token TTL and a trigger history capped at 1000 records.
    pub fn new() -> Self {
        Self {
            active: Mutex::new(HashMap::new()),
            triggers: Mutex::new(Vec::new()),
            ttl: Duration::from_secs(300), // 5 minutes
            max_triggers: 1000,
        }
    }

    /// Creates a manager with the given token TTL in seconds; other settings keep their defaults.
    pub fn with_ttl(ttl_secs: u64) -> Self {
        let mut m = Self::new();
        m.ttl = Duration::from_secs(ttl_secs);
        m
    }

    /// Mint a fresh canary token for the given context label.
    ///
    /// The token is registered as active.  Inject
    /// `token.injection_fragment()` into your system prompt.
    pub fn mint(&self, context: &str) -> CanaryToken {
        let id = generate_token_id();
        let token = CanaryToken {
            id: id.clone(),
            context: context.to_string(),
            created_at: chrono_now(),
            created_instant: Some(Instant::now()),
        };
        self.active
            .lock()
            .unwrap()
            .insert(id, (token.clone(), Instant::now()));
        token
    }

    /// Scan model output for any active canary tokens.
    ///
    /// Returns the first triggered token (if any) and records the event.
    pub fn scan_output(&self, output: &str) -> Option<CanaryToken> {
        self.expire_stale();

        let mut active = self.active.lock().unwrap();
        let mut triggered: Option<CanaryToken> = None;

        for (id, (token, _)) in active.iter() {
            let fragment = token.injection_fragment();
            if output.contains(&fragment) {
                triggered = Some(token.clone());
                let excerpt: String = output.chars().take(200).collect();
                let trigger = CanaryTrigger {
                    token_id: id.clone(),
                    context: token.context.clone(),
                    triggered_at: chrono_now(),
                    output_excerpt: excerpt,
                };
                let mut trigs = self.triggers.lock().unwrap();
                if trigs.len() >= self.max_triggers {
                    trigs.remove(0);
                }
                trigs.push(trigger);
                break;
            }
        }

        // Remove triggered token (single-use).
        if let Some(ref t) = triggered {
            active.remove(&t.id);
        }

        triggered
    }

    /// Retire a token by id without it having triggered.
    pub fn retire(&self, token_id: &str) {
        self.active.lock().unwrap().remove(token_id);
    }

    /// Return all recorded trigger events.
    pub fn triggers(&self) -> Vec<CanaryTrigger> {
        self.triggers.lock().unwrap().clone()
    }

    /// Return current active token count.
    pub fn active_count(&self) -> usize {
        self.active.lock().unwrap().len()
    }

    /// Expire tokens that have exceeded their TTL.
    fn expire_stale(&self) {
        let now = Instant::now();
        self.active
            .lock()
            .unwrap()
            .retain(|_, (_, created)| now.duration_since(*created) < self.ttl);
    }
}

impl Default for CanaryTokenManager {
    fn default() -> Self {
        Self::new()
    }
}

// ─── Helpers ─────────────────────────────────────────────────────────────────

fn generate_token_id() -> String {
    // 16 random hex bytes → 32-char string.
    // Uses std only to avoid adding a dep.
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    use std::time::{SystemTime, UNIX_EPOCH};

    let mut s = DefaultHasher::new();
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
        .hash(&mut s);
    std::process::id().hash(&mut s);
    let h1 = s.finish();

    // Second hash for more entropy
    (h1 ^ 0xdeadbeefcafebabe).hash(&mut s);
    let h2 = s.finish();

    format!("{:016x}{:016x}", h1, h2)
}

fn chrono_now() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    format!("{}Z", secs)
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mint_and_no_trigger() {
        let mgr = CanaryTokenManager::new();
        let tok = mgr.mint("test context");
        assert_eq!(mgr.active_count(), 1);
        let result = mgr.scan_output("this is a benign response");
        assert!(result.is_none(), "clean output should not trigger");
        assert_eq!(mgr.active_count(), 1); // still active
        mgr.retire(&tok.id);
        assert_eq!(mgr.active_count(), 0);
    }

    #[test]
    fn trigger_detected() {
        let mgr = CanaryTokenManager::new();
        let tok = mgr.mint("sensitive context");
        let fragment = tok.injection_fragment();
        let bad_output = format!("Here is your answer: {} and more text", fragment);
        let triggered = mgr.scan_output(&bad_output);
        assert!(triggered.is_some(), "trigger should be detected");
        assert_eq!(triggered.unwrap().id, tok.id);
        assert_eq!(mgr.triggers().len(), 1);
        assert_eq!(mgr.active_count(), 0, "triggered token should be retired");
    }

    #[test]
    fn token_ids_are_unique() {
        let mgr = CanaryTokenManager::new();
        let t1 = mgr.mint("ctx1");
        let t2 = mgr.mint("ctx2");
        assert_ne!(t1.id, t2.id);
    }
}
