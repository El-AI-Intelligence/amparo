// Originally part of Axiom-OS (MIT, Copyright (c) Pixel Phantom AI).
// Ported to Amparo and relicensed Apache-2.0 — see the repository NOTICE.

//! Amparo Privacy — per-request privacy policy engine
//!
//! Every inference request, engram store, and external API call is tagged with a
//! `PrivacyLevel` that governs where data may flow.  The privacy engine evaluates
//! the level against the active `PrivacyPolicy` and either allows, redacts, or
//! blocks the operation.

#![warn(missing_docs)]

pub mod canary;
pub mod ledger;
pub use canary::{CanaryToken, CanaryTokenManager, CanaryTrigger};
pub use ledger::{privacy_dir, site_host_only, LedgerKind, LedgerRow, LedgerStore, LedgerSummary};

use serde::{Deserialize, Serialize};
use thiserror::Error;

// ─── Privacy levels ──────────────────────────────────────────────────────────

/// How sensitive is this piece of data / this request?
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[derive(Default)]
pub enum PrivacyLevel {
    /// All processing stays on-device.  Nothing leaves the local machine.
    StrictLocal  = 0,
    /// May leave the device for user-controlled infrastructure (e.g. own VPS).
    Hybrid       = 1,
    /// May be sent to cloud providers (Ollama cloud, OpenAI, etc.).
    #[default]
    CloudFirst   = 2,
    /// Enterprise-managed: governed by tenant compliance mode.
    Enterprise   = 3,
}


impl std::fmt::Display for PrivacyLevel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::StrictLocal => write!(f, "strict_local"),
            Self::Hybrid      => write!(f, "hybrid"),
            Self::CloudFirst  => write!(f, "cloud_first"),
            Self::Enterprise  => write!(f, "enterprise"),
        }
    }
}

impl std::str::FromStr for PrivacyLevel {
    type Err = PrivacyError;
    fn from_str(s: &str) -> Result<Self> {
        match s {
            "strict_local" => Ok(Self::StrictLocal),
            "hybrid"       => Ok(Self::Hybrid),
            "cloud_first"  => Ok(Self::CloudFirst),
            "enterprise"   => Ok(Self::Enterprise),
            _              => Err(PrivacyError::InvalidLevel(s.to_string())),
        }
    }
}

// ─── Data categories ─────────────────────────────────────────────────────────

/// What kind of data is being handled?
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DataCategory {
    /// General conversation text.
    Chat,
    /// Personal facts, preferences, identity data.
    PersonalInfo,
    /// Health / medical information (HIPAA-sensitive).
    HealthData,
    /// Financial records, transactions, wallet data.
    FinancialData,
    /// Biometric or sensor data (face, voice, location).
    BiometricData,
    /// Code, files, project data.
    CodeData,
    /// Research or OSINT data.
    ResearchData,
    /// System telemetry (non-personal).
    Telemetry,
}

// ─── Privacy policy ──────────────────────────────────────────────────────────

/// User-defined privacy policy that the engine enforces.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PrivacyPolicy {
    /// Default privacy level for requests that don't specify one.
    pub default_level: PrivacyLevel,
    /// Per-category overrides.  E.g. HealthData → StrictLocal even if default is CloudFirst.
    pub category_overrides: Vec<CategoryOverride>,
    /// Domains that are explicitly blocked from receiving data.
    pub blocked_domains: Vec<String>,
    /// Domains that are explicitly allowed (allowlist mode).
    pub allowed_domains: Vec<String>,
    /// Whether to strip PII before any cloud request.
    pub auto_redact_pii: bool,
    /// Layer 3.5: When true, ALL inference stays local — cloud fallback is
    /// disabled even if API keys are configured. This is a stronger guarantee
    /// than offline_mode (which suppresses all outbound calls); it specifically
    /// targets inference routing.
    #[serde(default)]
    pub local_inference_only: bool,
}

impl Default for PrivacyPolicy {
    fn default() -> Self {
        Self {
            default_level: PrivacyLevel::CloudFirst,
            category_overrides: vec![
                CategoryOverride { category: DataCategory::HealthData, level: PrivacyLevel::StrictLocal },
                CategoryOverride { category: DataCategory::FinancialData, level: PrivacyLevel::Hybrid },
                CategoryOverride { category: DataCategory::BiometricData, level: PrivacyLevel::StrictLocal },
            ],
            blocked_domains: Vec::new(),
            allowed_domains: Vec::new(),
            auto_redact_pii: true,
            local_inference_only: false,
        }
    }
}

/// A per-category override of the default privacy level.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CategoryOverride {
    /// The data category this override applies to.
    pub category: DataCategory,
    /// The privacy level to enforce for that category.
    pub level: PrivacyLevel,
}

// ─── Decision engine ─────────────────────────────────────────────────────────

/// The result of a privacy evaluation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PrivacyDecision {
    /// Whether the request is permitted under the active policy.
    pub allowed: bool,
    /// The privacy level that was effectively applied to the request.
    pub effective_level: PrivacyLevel,
    /// Whether PII should be redacted from the request before it leaves the device.
    pub redact_pii: bool,
    /// Human-readable explanation of the decision.
    pub reason: String,
}

/// Evaluate whether a request is permitted under the active policy.
pub fn evaluate(
    policy: &PrivacyPolicy,
    request_level: Option<PrivacyLevel>,
    category: DataCategory,
    target_domain: Option<&str>,
) -> PrivacyDecision {
    // 1. Resolve effective level: per-category override > request-level > default
    let effective_level = policy
        .category_overrides
        .iter()
        .find(|o| o.category == category)
        .map(|o| o.level)
        .or(request_level)
        .unwrap_or(policy.default_level);

    // 2. Domain checks
    if let Some(domain) = target_domain {
        if policy.blocked_domains.iter().any(|d| domain.contains(d)) {
            return PrivacyDecision {
                allowed: false,
                effective_level,
                redact_pii: false,
                reason: format!("Domain '{}' is blocked by privacy policy", domain),
            };
        }
        if !policy.allowed_domains.is_empty()
            && !policy.allowed_domains.iter().any(|d| domain.contains(d))
        {
            return PrivacyDecision {
                allowed: false,
                effective_level,
                redact_pii: false,
                reason: format!("Domain '{}' not in allowlist", domain),
            };
        }
    }

    // 3. StrictLocal blocks any cloud target
    if effective_level == PrivacyLevel::StrictLocal && target_domain.is_some() {
        let domain = target_domain.unwrap_or("unknown");
        let is_local = domain.contains("localhost")
            || domain.contains("127.0.0.1")
            || domain.contains("::1");
        if !is_local {
            return PrivacyDecision {
                allowed: false,
                effective_level,
                redact_pii: false,
                reason: "StrictLocal: cannot send data to remote endpoints".to_string(),
            };
        }
    }

    // 4. Determine PII redaction
    let redact_pii = policy.auto_redact_pii
        && effective_level >= PrivacyLevel::CloudFirst
        && matches!(category, DataCategory::Chat | DataCategory::PersonalInfo);

    PrivacyDecision {
        allowed: true,
        effective_level,
        redact_pii,
        reason: "Permitted by privacy policy".to_string(),
    }
}

// ─── Errors ──────────────────────────────────────────────────────────────────

/// Errors produced by the privacy engine.
#[derive(Error, Debug)]
pub enum PrivacyError {
    /// An unrecognised privacy level string was parsed.
    #[error("Invalid privacy level: {0}")]
    InvalidLevel(String),
    /// The operation was blocked by the active privacy policy.
    #[error("Policy violation: {0}")]
    PolicyViolation(String),
}

/// Result type for privacy engine operations.
pub type Result<T> = std::result::Result<T, PrivacyError>;

// ─── Secure Minions Protocol ─────────────────────────────────────────────────
//
// The "Secure Minions" pattern (from Ollama privacy architecture) enables
// hybrid cloud inference while preserving user privacy:
//
//   1. Strip PII from the request locally (on-device)
//   2. Send the abstract, anonymised query to the cloud model
//   3. Receive the cloud's response
//   4. Re-contextualise locally: restore PII/user context into the response
//
// This allows powerful cloud models to be used for reasoning while keeping
// personal data entirely on-device.

/// A PII placeholder used during redaction / restoration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PiiPlaceholder {
    /// The placeholder token inserted into the text (e.g. `[NAME_1]`).
    pub token: String,
    /// The original value that was redacted.
    pub original: String,
    /// Category of the PII.
    pub category: String,
}

/// Result of the Secure Minions pre-processing step.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SecureMinionsRequest {
    /// The sanitised text with PII replaced by placeholders.
    pub sanitised_text: String,
    /// Mapping of placeholders to their original values (kept on-device only).
    pub pii_map: Vec<PiiPlaceholder>,
    /// Whether any PII was actually found and redacted.
    pub pii_found: bool,
}

/// Strip PII from text using pattern-based heuristics.
/// Returns the sanitised text and a map to restore the original.
pub fn secure_minions_strip(text: &str) -> SecureMinionsRequest {
    let mut sanitised = text.to_string();
    let mut pii_map = Vec::new();
    let mut counter = 0u32;

    // Email pattern
    let email_re = regex::Regex::new(r"\b[A-Za-z0-9._%+-]+@[A-Za-z0-9.-]+\.[A-Z|a-z]{2,}\b")
        .unwrap();
    for cap in email_re.find_iter(text) {
        counter += 1;
        let token = format!("[EMAIL_{}]", counter);
        pii_map.push(PiiPlaceholder {
            token: token.clone(),
            original: cap.as_str().to_string(),
            category: "email".to_string(),
        });
        sanitised = sanitised.replacen(cap.as_str(), &token, 1);
    }

    // Phone pattern (US-style)
    let phone_re = regex::Regex::new(
        r"\b(?:\+?1[-.\s]?)?\(?[0-9]{3}\)?[-.\s]?[0-9]{3}[-.\s]?[0-9]{4}\b",
    ).unwrap();
    for cap in phone_re.find_iter(text) {
        counter += 1;
        let token = format!("[PHONE_{}]", counter);
        pii_map.push(PiiPlaceholder {
            token: token.clone(),
            original: cap.as_str().to_string(),
            category: "phone".to_string(),
        });
        sanitised = sanitised.replacen(cap.as_str(), &token, 1);
    }

    // SSN pattern
    let ssn_re = regex::Regex::new(r"\b\d{3}-\d{2}-\d{4}\b").unwrap();
    for cap in ssn_re.find_iter(text) {
        counter += 1;
        let token = format!("[SSN_{}]", counter);
        pii_map.push(PiiPlaceholder {
            token: token.clone(),
            original: cap.as_str().to_string(),
            category: "ssn".to_string(),
        });
        sanitised = sanitised.replacen(cap.as_str(), &token, 1);
    }

    // Credit card pattern (16-digit with separators)
    let cc_re = regex::Regex::new(
        r"\b\d{4}[- ]\d{4}[- ]\d{4}[- ]\d{4}\b",
    ).unwrap();
    for cap in cc_re.find_iter(text) {
        counter += 1;
        let token = format!("[CREDITCARD_{}]", counter);
        pii_map.push(PiiPlaceholder {
            token: token.clone(),
            original: cap.as_str().to_string(),
            category: "credit_card".to_string(),
        });
        sanitised = sanitised.replacen(cap.as_str(), &token, 1);
    }

    // Password contextual patterns: "password is X", "password: X", "my password X"
    let pwd_re = regex::Regex::new(
        r"(?i)\b(my\s+)?password\s*(?:is|:|=)\s*(\S+)",
    ).unwrap();
    for cap in pwd_re.captures_iter(text) {
        if let Some(m) = cap.get(0) {
            counter += 1;
            let token = format!("[PASSWORD_{}]", counter);
            pii_map.push(PiiPlaceholder {
                token: token.clone(),
                original: m.as_str().to_string(),
                category: "password".to_string(),
            });
            sanitised = sanitised.replacen(m.as_str(), &token, 1);
        }
    }

    // Address contextual patterns: "my address is X", "I live at X"
    let addr_re = regex::Regex::new(
        r"(?i)\b(?:my address is|i live at|address:)\s+([^\n.]{5,60})",
    ).unwrap();
    for cap in addr_re.captures_iter(text) {
        if let Some(m) = cap.get(0) {
            counter += 1;
            let token = format!("[ADDRESS_{}]", counter);
            pii_map.push(PiiPlaceholder {
                token: token.clone(),
                original: m.as_str().to_string(),
                category: "address".to_string(),
            });
            sanitised = sanitised.replacen(m.as_str(), &token, 1);
        }
    }

    // Medical contextual patterns: "I have [condition]", "diagnosed with [condition]"
    let medical_re = regex::Regex::new(
        r"(?i)\b(?:i have|diagnosed with|i suffer from|my condition is)\s+([a-z][^\n.]{2,50})",
    ).unwrap();
    for cap in medical_re.captures_iter(text) {
        if let Some(m) = cap.get(0) {
            counter += 1;
            let token = format!("[MEDICAL_{}]", counter);
            pii_map.push(PiiPlaceholder {
                token: token.clone(),
                original: m.as_str().to_string(),
                category: "medical".to_string(),
            });
            sanitised = sanitised.replacen(m.as_str(), &token, 1);
        }
    }

    SecureMinionsRequest {
        pii_found: !pii_map.is_empty(),
        sanitised_text: sanitised,
        pii_map,
    }
}

/// Re-contextualise a cloud response by restoring PII from the local map.
pub fn secure_minions_restore(response: &str, pii_map: &[PiiPlaceholder]) -> String {
    let mut restored = response.to_string();
    for placeholder in pii_map {
        restored = restored.replace(&placeholder.token, &placeholder.original);
    }
    restored
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_policy_blocks_health_to_cloud() {
        let policy = PrivacyPolicy::default();
        let decision = evaluate(&policy, None, DataCategory::HealthData, Some("api.openai.com"));
        assert!(!decision.allowed);
        assert_eq!(decision.effective_level, PrivacyLevel::StrictLocal);
    }

    #[test]
    fn default_policy_allows_chat_to_cloud() {
        let policy = PrivacyPolicy::default();
        let decision = evaluate(&policy, None, DataCategory::Chat, Some("api.openai.com"));
        assert!(decision.allowed);
        assert!(decision.redact_pii);
    }

    #[test]
    fn blocked_domain_prevents_request() {
        let policy = PrivacyPolicy {
            blocked_domains: vec!["evil.com".to_string()],
            ..Default::default()
        };
        let decision = evaluate(&policy, None, DataCategory::Chat, Some("https://evil.com/api"));
        assert!(!decision.allowed);
    }

    #[test]
    fn strict_local_allows_localhost() {
        let policy = PrivacyPolicy::default();
        let decision = evaluate(
            &policy,
            Some(PrivacyLevel::StrictLocal),
            DataCategory::CodeData,
            Some("http://localhost:11434"),
        );
        assert!(decision.allowed);
    }

    #[test]
    fn parse_privacy_level() {
        assert_eq!("strict_local".parse::<PrivacyLevel>().unwrap(), PrivacyLevel::StrictLocal);
        assert_eq!("cloud_first".parse::<PrivacyLevel>().unwrap(), PrivacyLevel::CloudFirst);
        assert!("invalid".parse::<PrivacyLevel>().is_err());
    }

    #[test]
    fn secure_minions_strip_email() {
        let text = "Send it to alice@example.com please";
        let result = secure_minions_strip(text);
        assert!(result.pii_found);
        assert!(!result.sanitised_text.contains("alice@example.com"));
        assert!(result.sanitised_text.contains("[EMAIL_"));
        assert_eq!(result.pii_map.len(), 1);
        assert_eq!(result.pii_map[0].category, "email");
    }

    #[test]
    fn secure_minions_strip_phone() {
        let text = "Call me at 555-123-4567";
        let result = secure_minions_strip(text);
        assert!(result.pii_found);
        assert!(!result.sanitised_text.contains("555-123-4567"));
    }

    #[test]
    fn secure_minions_strip_ssn() {
        let text = "My SSN is 123-45-6789";
        let result = secure_minions_strip(text);
        assert!(result.pii_found);
        assert!(!result.sanitised_text.contains("123-45-6789"));
        assert!(result.pii_map.iter().any(|p| p.category == "ssn"));
    }

    #[test]
    fn secure_minions_no_pii() {
        let text = "The weather today is sunny";
        let result = secure_minions_strip(text);
        assert!(!result.pii_found);
        assert_eq!(result.sanitised_text, text);
    }

    #[test]
    fn secure_minions_restore_roundtrip() {
        let text = "Contact alice@example.com or call 555-123-4567";
        let stripped = secure_minions_strip(text);
        let response = format!("I'll reach out to {} right away", stripped.sanitised_text);
        let restored = secure_minions_restore(&response, &stripped.pii_map);
        assert!(restored.contains("alice@example.com"));
        assert!(restored.contains("555-123-4567"));
    }
}
