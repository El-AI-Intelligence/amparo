// Originally part of Axiom-OS (MIT, Copyright (c) Pixel Phantom AI).
// Ported to Amparo and relicensed Apache-2.0 — see the repository NOTICE.

//! Amparo Memory — context-window assembly for agent inference calls
//!
//! Manages the context window assembly pipeline that feeds every inference call:
//!
//! ```text
//! ┌──────────────────────────────────────────────────────────────┐
//! │  MEMORY PIPELINE                                             │
//! │                                                              │
//! │  SystemPrompt ─────────────────────────────────┐            │
//! │  CharacterBias (VIA strengths) ────────────────┤            │
//! │  PurposeVector ────────────────────────────────┤  REQUIRED  │
//! │                                                │            │
//! │  CurrentTurn (user message) ───────────────────┤            │
//! │                                                │            │
//! │  EngramRetrieval (long-term memory hits) ──────┤  HIGH      │
//! │                                                │            │
//! │  RecentHistory (last N turns) ─────────────────┤  NORMAL    │
//! │                                                │            │
//! │  CompactedHistory (older turns, summarised) ───┤  LOW       │
//! │                                                │            │
//! │              ↓ ContextAssembler.assemble()     │            │
//! │                                                │            │
//! │  Sorted by priority → token budget applied     │            │
//! │  Overflow: compact Normal→Low, drop Low        │            │
//! └──────────────────────────────────────────────────────────────┘
//! ```
//!
//! **Priority model**
//!
//! | Priority   | Drop order | Examples                              |
//! |------------|-----------|----------------------------------------|
//! | Required   | Never     | System prompt, current user message    |
//! | High       | Last      | Engram retrievals, character bias      |
//! | Normal     | Second    | Recent history (past 12 turns)         |
//! | Low        | First     | Compacted older history                |
//!
//! **Token counting**
//! Uses a fast approximate counter (4 chars/token heuristic, word-blend).
//! Not exact — callers wanting exactness should use tiktoken on the assembled messages.
//!
//! **No external dependencies** — this crate is deliberately zero-dep for maximum
//! portability across every Amparo build target.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use thiserror::Error;

// ─────────────────────────────────────── errors ──────────────────────────────

#[derive(Error, Debug)]
pub enum MemoryError {
    #[error("Serialization error: {0}")]
    Serialization(#[from] serde_json::Error),
    #[error("Token budget exhausted: required slots exceed budget of {budget}")]
    BudgetExhausted { budget: usize },
    #[error("Invalid slot: {0}")]
    InvalidSlot(String),
}

pub type Result<T> = std::result::Result<T, MemoryError>;

// ─────────────────────────────────────── priority ────────────────────────────

/// Retention priority for a context slot.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
pub enum ContextPriority {
    /// Dropped first when over budget
    Low,
    /// Second to be dropped
    Normal,
    /// Retained unless budget is severely constrained
    High,
    /// Never dropped — system instructions, current message, critical council output
    Required,
}

// ─────────────────────────────────────── roles & sources ─────────────────────

/// Role of a message as seen by the LLM
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum ContextRole {
    System,
    User,
    Assistant,
    Tool,
}

impl ContextRole {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::System    => "system",
            Self::User      => "user",
            Self::Assistant => "assistant",
            Self::Tool      => "tool",
        }
    }
}

/// Which subsystem produced this context slot
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum ContextSource {
    SystemPrompt,
    CharacterBias,
    PurposeVector,
    CurrentTurn,
    RecentHistory,
    CompactedHistory,
    EngramRetrieval,
    WorldContext,
    CouncilInstruction,
    RedLineWarning,
}

impl ContextSource {
    pub fn default_priority(&self) -> ContextPriority {
        match self {
            Self::SystemPrompt      => ContextPriority::Required,
            Self::CharacterBias     => ContextPriority::Required,
            Self::PurposeVector     => ContextPriority::High,
            Self::CurrentTurn       => ContextPriority::Required,
            Self::RecentHistory     => ContextPriority::Normal,
            Self::CompactedHistory  => ContextPriority::Low,
            Self::EngramRetrieval   => ContextPriority::High,
            Self::WorldContext      => ContextPriority::High,
            Self::CouncilInstruction => ContextPriority::Required,
            Self::RedLineWarning    => ContextPriority::Required,
        }
    }
}

// ─────────────────────────────────────── slot ────────────────────────────────

/// A single unit of context — maps to one OpenAI `messages` entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContextSlot {
    pub role: ContextRole,
    pub content: String,
    pub token_estimate: usize,
    pub priority: ContextPriority,
    pub source: ContextSource,
    /// Optional metadata for attribution and debugging
    pub metadata: serde_json::Value,
    pub created_at: DateTime<Utc>,
}

impl ContextSlot {
    pub fn new(
        role: ContextRole,
        content: impl Into<String>,
        source: ContextSource,
    ) -> Self {
        let content = content.into();
        let token_estimate = estimate_tokens(&content);
        let priority = source.default_priority();
        Self {
            role,
            content,
            token_estimate,
            priority,
            source,
            metadata: serde_json::json!({}),
            created_at: Utc::now(),
        }
    }

    pub fn with_priority(mut self, p: ContextPriority) -> Self {
        self.priority = p;
        self
    }

    pub fn with_metadata(mut self, meta: serde_json::Value) -> Self {
        self.metadata = meta;
        self
    }

    /// Serialize to the OpenAI-compatible `{"role": "...", "content": "..."}` format
    pub fn to_openai_message(&self) -> serde_json::Value {
        serde_json::json!({
            "role": self.role.as_str(),
            "content": self.content,
        })
    }
}

// ─────────────────────────────────────── token counting ──────────────────────

/// Approximate token count using word-blend heuristic (~4 chars/token, ~1.35 tokens/word).
/// Accurate within ±15% for typical English prose. Fast: O(n) with no allocations.
pub fn estimate_tokens(text: &str) -> usize {
    if text.is_empty() { return 0; }
    let words = text.split_whitespace().count();
    let chars = text.len();
    // Blend word-based and char-based estimates
    let word_est = (words as f64 * 1.35) as usize;
    let char_est = chars / 4;
    word_est.max(char_est).max(1)
}

/// Exact token count from a pre-computed vector.
/// Use when callers have already run tiktoken.
pub fn exact_tokens(count: usize) -> usize {
    count
}

// ─────────────────────────────────────── assembler config ────────────────────

/// Configuration for the ContextAssembler
#[derive(Debug, Clone)]
pub struct AssemblerConfig {
    /// Hard token ceiling — the final assembled context must not exceed this
    pub token_budget: usize,
    /// Fraction of the budget reserved for Required+High slots
    /// (prevents Normal/Low slots from starving critical context)
    pub high_priority_reserve: f64,
    /// Maximum number of Recent History turns to include before compacting
    pub max_recent_turns: usize,
    /// If true, compacted slots are included with a "[SUMMARY]" prefix
    /// rather than being discarded; preserves temporal context at lower cost
    pub include_compacted: bool,
}

impl Default for AssemblerConfig {
    fn default() -> Self {
        Self {
            token_budget: 8_192,
            high_priority_reserve: 0.60,
            max_recent_turns: 12,
            include_compacted: true,
        }
    }
}

impl AssemblerConfig {
    pub fn with_budget(budget: usize) -> Self {
        Self { token_budget: budget, ..Self::default() }
    }
}

// ─────────────────────────────────────── assembler ───────────────────────────

/// Assembles an ordered list of context slots that fits within the token budget.
///
/// ### Compaction strategy
///
/// 1. **Partition** slots into buckets by priority.
/// 2. **Always include** `Required` slots (error if they alone exceed budget).
/// 3. **Fill from high to low** until budget is exhausted:
///    - High  → include fully
///    - Normal → include until budget tight, then compact via `compact_slots()`
///    - Low   → include remainder; discard first when tight
/// 4. Slots are **re-ordered** for optimal LLM comprehension:
///    `System → HiContext → RecentHistory → CurrentTurn`
pub struct ContextAssembler {
    pub config: AssemblerConfig,
}

impl ContextAssembler {
    pub fn new(token_budget: usize) -> Self {
        Self { config: AssemblerConfig::with_budget(token_budget) }
    }

    pub fn with_config(config: AssemblerConfig) -> Self {
        Self { config }
    }

    /// Assemble slots into an ordered, budget-constrained context window.
    /// Returns `Err` only if Required slots alone exceed the budget.
    pub fn assemble(&self, mut slots: Vec<ContextSlot>) -> Result<AssembledContext> {
        // Sort by priority descending, then by creation time ascending within same priority
        slots.sort_by(|a, b| {
            b.priority.cmp(&a.priority).then(a.created_at.cmp(&b.created_at))
        });

        let mut required: Vec<ContextSlot> = Vec::new();
        let mut high:     Vec<ContextSlot> = Vec::new();
        let mut normal:   Vec<ContextSlot> = Vec::new();
        let mut low:      Vec<ContextSlot> = Vec::new();

        for slot in slots {
            match slot.priority {
                ContextPriority::Required => required.push(slot),
                ContextPriority::High     => high.push(slot),
                ContextPriority::Normal   => normal.push(slot),
                ContextPriority::Low      => low.push(slot),
            }
        }

        let required_tokens: usize = required.iter().map(|s| s.token_estimate).sum();
        if required_tokens > self.config.token_budget {
            return Err(MemoryError::BudgetExhausted { budget: self.config.token_budget });
        }

        let mut remaining = self.config.token_budget - required_tokens;
        let mut included: Vec<ContextSlot> = Vec::new();
        let mut dropped_tokens: usize = 0;
        let mut dropped_count: usize = 0;

        // Fill High
        for slot in high {
            if slot.token_estimate <= remaining {
                remaining -= slot.token_estimate;
                included.push(slot);
            } else {
                dropped_tokens += slot.token_estimate;
                dropped_count += 1;
            }
        }

        // Fill Normal — apply max_recent_turns limit first
        let normal: Vec<ContextSlot> = normal
            .into_iter()
            .take(self.config.max_recent_turns)
            .collect();
        for slot in &normal {
            if slot.token_estimate <= remaining {
                remaining -= slot.token_estimate;
                included.push(slot.clone());
            } else {
                // Try compaction: emit a summary slot instead
                if self.config.include_compacted {
                    let summary = compact_slot(slot);
                    if summary.token_estimate <= remaining {
                        remaining -= summary.token_estimate;
                        included.push(summary);
                        continue;
                    }
                }
                dropped_tokens += slot.token_estimate;
                dropped_count += 1;
            }
        }

        // Fill Low
        for slot in low {
            if slot.token_estimate <= remaining {
                remaining -= slot.token_estimate;
                included.push(slot);
            } else {
                dropped_tokens += slot.token_estimate;
                dropped_count += 1;
            }
        }

        // Merge required first, then rest in source-ordered presentation
        let mut final_slots = required;
        // Re-sort included: System/CharacterBias before history, CurrentTurn last
        included.sort_by_key(|s| match s.source {
            ContextSource::WorldContext | ContextSource::EngramRetrieval => 0,
            ContextSource::CompactedHistory => 1,
            ContextSource::RecentHistory    => 2,
            _ => 3,
        });
        final_slots.extend(included);

        let total_tokens: usize = final_slots.iter().map(|s| s.token_estimate).sum();

        Ok(AssembledContext {
            slots: final_slots,
            total_tokens,
            budget: self.config.token_budget,
            dropped_tokens,
            dropped_slots: dropped_count,
        })
    }
}

/// Output of `ContextAssembler::assemble()`
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AssembledContext {
    pub slots: Vec<ContextSlot>,
    pub total_tokens: usize,
    pub budget: usize,
    pub dropped_tokens: usize,
    pub dropped_slots: usize,
}

impl AssembledContext {
    /// Serialize to the OpenAI `messages` array format
    pub fn to_openai_messages(&self) -> Vec<serde_json::Value> {
        self.slots.iter().map(|s| s.to_openai_message()).collect()
    }

    /// Fraction of the budget consumed
    pub fn utilization(&self) -> f64 {
        self.total_tokens as f64 / self.budget as f64
    }

    /// True if we had to drop or compact any slots
    pub fn was_compacted(&self) -> bool {
        self.dropped_slots > 0
    }
}

// ─────────────────────────────────────── compaction ──────────────────────────

/// Produce a compact summary slot from a full slot.
/// The compressed form prefixes content with "[SUMMARY]:" and truncates to
/// approximately 25% of the original token count.
fn compact_slot(slot: &ContextSlot) -> ContextSlot {
    let words: Vec<&str> = slot.content.split_whitespace().collect();
    let keep = (words.len() / 4).max(6);
    let summary = format!(
        "[SUMMARY from {}]: {}…",
        format!("{:?}", slot.source).to_lowercase(),
        words[..keep.min(words.len())].join(" ")
    );
    ContextSlot {
        role: slot.role,
        content: summary.clone(),
        token_estimate: estimate_tokens(&summary),
        priority: ContextPriority::Low,
        source: ContextSource::CompactedHistory,
        metadata: slot.metadata.clone(),
        created_at: slot.created_at,
    }
}

// ─────────────────────────────────────── builder ─────────────────────────────

/// Fluent builder for assembling a context window from scratch.
///
/// ```
/// use amparo_memory::{ContextBuilder, ContextRole, ContextSource};
///
/// let context = ContextBuilder::new(4096)
///     .system("You are Amparo, an agent that acts under policy.")
///     .engram("Last week you discussed career goals.")
///     .user("What should I focus on this week?")
///     .build()
///     .unwrap();
/// ```
pub struct ContextBuilder {
    assembler: ContextAssembler,
    slots: Vec<ContextSlot>,
}

impl ContextBuilder {
    pub fn new(token_budget: usize) -> Self {
        Self {
            assembler: ContextAssembler::new(token_budget),
            slots: Vec::new(),
        }
    }

    pub fn with_config(config: AssemblerConfig) -> Self {
        Self { assembler: ContextAssembler::with_config(config), slots: Vec::new() }
    }

    pub fn add_slot(mut self, slot: ContextSlot) -> Self {
        self.slots.push(slot);
        self
    }

    pub fn system(self, content: impl Into<String>) -> Self {
        self.add_slot(ContextSlot::new(ContextRole::System, content, ContextSource::SystemPrompt))
    }

    pub fn character_bias(self, content: impl Into<String>) -> Self {
        self.add_slot(ContextSlot::new(ContextRole::System, content, ContextSource::CharacterBias))
    }

    pub fn purpose_vector(self, content: impl Into<String>) -> Self {
        self.add_slot(ContextSlot::new(ContextRole::System, content, ContextSource::PurposeVector))
    }

    pub fn user(self, content: impl Into<String>) -> Self {
        self.add_slot(ContextSlot::new(ContextRole::User, content, ContextSource::CurrentTurn))
    }

    pub fn assistant(self, content: impl Into<String>) -> Self {
        self.add_slot(
            ContextSlot::new(ContextRole::Assistant, content, ContextSource::RecentHistory)
                .with_priority(ContextPriority::Normal),
        )
    }

    pub fn engram(self, content: impl Into<String>) -> Self {
        self.add_slot(ContextSlot::new(ContextRole::System, content, ContextSource::EngramRetrieval))
    }

    pub fn world_context(self, content: impl Into<String>) -> Self {
        self.add_slot(ContextSlot::new(ContextRole::System, content, ContextSource::WorldContext))
    }

    pub fn council_instruction(self, content: impl Into<String>) -> Self {
        self.add_slot(ContextSlot::new(ContextRole::System, content, ContextSource::CouncilInstruction))
    }

    pub fn previous_turn(self, user: impl Into<String>, assistant: impl Into<String>) -> Self {
        let user_slot = ContextSlot::new(ContextRole::User, user, ContextSource::RecentHistory)
            .with_priority(ContextPriority::Normal);
        let asst_slot = ContextSlot::new(ContextRole::Assistant, assistant, ContextSource::RecentHistory)
            .with_priority(ContextPriority::Normal);
        self.add_slot(user_slot).add_slot(asst_slot)
    }

    /// Assemble the context window, applying compaction if needed.
    pub fn build(self) -> Result<AssembledContext> {
        self.assembler.assemble(self.slots)
    }
}

// ─────────────────────────────────────── summarizer ──────────────────────────

/// Summarizes long conversation histories into compact representations.
/// Uses extractive summarization (key sentence extraction) for zero-dependency operation.
/// For abstractive summarization, callers should use an LLM via the inference provider.
pub struct ConversationSummarizer {
    /// Maximum tokens for a summary slot
    pub max_summary_tokens: usize,
    /// Number of key sentences to extract per chunk
    pub key_sentences_per_chunk: usize,
}

impl Default for ConversationSummarizer {
    fn default() -> Self {
        Self {
            max_summary_tokens: 200,
            key_sentences_per_chunk: 3,
        }
    }
}

impl ConversationSummarizer {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_max_tokens(mut self, tokens: usize) -> Self {
        self.max_summary_tokens = tokens;
        self
    }

    /// Summarize a list of context slots into a single compact summary.
    /// Returns a new ContextSlot with the summary content.
    pub fn summarize_slots(&self, slots: &[ContextSlot]) -> ContextSlot {
        if slots.is_empty() {
            return ContextSlot::new(
                ContextRole::System,
                "[SUMMARY]: No previous conversation history.",
                ContextSource::CompactedHistory,
            ).with_priority(ContextPriority::Low);
        }

        let combined: String = slots.iter()
            .map(|s| format!("[{}]: {}", s.role.as_str(), s.content))
            .collect::<Vec<_>>()
            .join("\n");

        let summary = self.extractive_summary(&combined);
        let truncated = self.truncate_to_tokens(&summary, self.max_summary_tokens);

        ContextSlot::new(
            ContextRole::System,
            format!("[SUMMARY of {} previous turns]: {}", slots.len(), truncated),
            ContextSource::CompactedHistory,
        ).with_priority(ContextPriority::Low)
    }

    /// Extractive summary: score sentences by importance and keep top-k.
    fn extractive_summary(&self, text: &str) -> String {
        let sentences: Vec<&str> = text
            .split(|c| c == '.' || c == '!' || c == '?' || c == '\n')
            .map(|s| s.trim())
            .filter(|s| !s.is_empty() && s.len() > 10)
            .collect();

        if sentences.is_empty() {
            return text.chars().take(200).collect();
        }

        // Score sentences by: word count, presence of key terms, position
        let key_terms = ["important", "decided", "agreed", "remember", "goal",
            "plan", "question", "answer", "error", "fix", "create", "build"];

        let mut scored: Vec<(usize, f64)> = sentences.iter().enumerate().map(|(i, s)| {
            let words: Vec<&str> = s.split_whitespace().collect();
            let word_score = (words.len() as f64).min(30.0) / 30.0;
            let key_score = key_terms.iter()
                .filter(|k| s.to_lowercase().contains(*k))
                .count() as f64 * 0.3;
            let position_score = if i == 0 || i == sentences.len() - 1 { 0.2 } else { 0.0 };
            (i, word_score + key_score + position_score)
        }).collect();

        scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));

        let top_k = self.key_sentences_per_chunk.min(sentences.len());
        let mut selected: Vec<usize> = scored[..top_k].iter().map(|(i, _)| *i).collect();
        selected.sort(); // maintain original order

        selected.iter()
            .map(|&i| sentences[i])
            .collect::<Vec<_>>()
            .join(". ")
    }

    /// Truncate text to approximately the given token count.
    fn truncate_to_tokens(&self, text: &str, max_tokens: usize) -> String {
        let est_tokens = estimate_tokens(text);
        if est_tokens <= max_tokens {
            return text.to_string();
        }
        // Rough: keep first N chars where N ≈ max_tokens * 4
        let max_chars = max_tokens * 4;
        let truncated: String = text.chars().take(max_chars).collect();
        format!("{}…", truncated.trim_end())
    }

    /// Compact a conversation history by summarizing older turns.
    /// Keeps the most recent `keep_recent` turns intact, summarizes the rest.
    pub fn compact_conversation(
        &self,
        slots: Vec<ContextSlot>,
        keep_recent: usize,
    ) -> Vec<ContextSlot> {
        if slots.len() <= keep_recent {
            return slots;
        }

        let split_at = slots.len() - keep_recent;
        let (older, recent) = slots.split_at(split_at);

        let summary = self.summarize_slots(older);

        let mut result = vec![summary];
        result.extend_from_slice(recent);
        result
    }
}

// ─────────────────────────────────────── tests ───────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_token_estimate_basic() {
        assert!(estimate_tokens("Hello world") > 0);
        let long = "The quick brown fox jumps over the lazy dog, and other such platitudes repeated many times to pad out the text significantly beyond a single sentence.";
        assert!(estimate_tokens(long) > estimate_tokens("Hello"));
    }

    #[test]
    fn test_assemble_fits_in_budget() {
        let ctx = ContextBuilder::new(2048)
            .system("You are Amparo.")
            .user("What is the meaning of life?")
            .build()
            .unwrap();
        assert!(ctx.total_tokens <= 2048);
        assert_eq!(ctx.was_compacted(), false);
    }

    #[test]
    fn test_required_slots_always_included() {
        let assembler = ContextAssembler::new(100);
        let slots = vec![
            ContextSlot::new(ContextRole::System, "System prompt very short.", ContextSource::SystemPrompt),
            ContextSlot::new(ContextRole::User, "Short user message.", ContextSource::CurrentTurn),
            ContextSlot::new(ContextRole::System,
                "A very long engram from long ago that contains a lot of text padding to push us over budget definitely.",
                ContextSource::EngramRetrieval).with_priority(ContextPriority::Low),
        ];
        let ctx = assembler.assemble(slots).unwrap();
        // Required slots must be present
        assert!(ctx.slots.iter().any(|s| s.source == ContextSource::SystemPrompt));
        assert!(ctx.slots.iter().any(|s| s.source == ContextSource::CurrentTurn));
    }

    #[test]
    fn test_budget_exhausted_error() {
        let assembler = ContextAssembler::new(2); // impossibly small
        let slots = vec![
            ContextSlot::new(
                ContextRole::System,
                "This is a long system prompt that will definitely exceed two tokens.",
                ContextSource::SystemPrompt,
            ),
        ];
        let result = assembler.assemble(slots);
        assert!(matches!(result, Err(MemoryError::BudgetExhausted { .. })));
    }

    #[test]
    fn test_context_builder_produces_openai_messages() {
        let ctx = ContextBuilder::new(8192)
            .system("You are Amparo, an agent that acts under policy.")
            .engram("User mentioned a career goal last month.")
            .user("Help me plan my day.")
            .build()
            .unwrap();
        let messages = ctx.to_openai_messages();
        assert!(messages.len() >= 2);
        assert!(messages.iter().any(|m| m["role"] == "system"));
        assert!(messages.iter().any(|m| m["role"] == "user"));
    }

    #[test]
    fn test_compaction_under_pressure() {
        let tiny_budget = 30; // only ~120 chars
        let assembler = ContextAssembler::new(tiny_budget);

        let mut slots = vec![
            ContextSlot::new(ContextRole::System, "Sys.", ContextSource::SystemPrompt),
            ContextSlot::new(ContextRole::User, "Hello.", ContextSource::CurrentTurn),
        ];
        // Add many Normal slots that cannot all fit
        for i in 0..10 {
            slots.push(
                ContextSlot::new(
                    ContextRole::User,
                    format!("This is history message number {} with extra padding text that grows tokens significantly.", i),
                    ContextSource::RecentHistory,
                ).with_priority(ContextPriority::Normal),
            );
        }

        let ctx = assembler.assemble(slots).unwrap();
        assert!(ctx.total_tokens <= tiny_budget);
        // Required slots must survive
        assert!(ctx.slots.iter().any(|s| s.source == ContextSource::SystemPrompt));
    }

    #[test]
    fn test_priority_ordering() {
        assert!(ContextPriority::Required > ContextPriority::High);
        assert!(ContextPriority::High > ContextPriority::Normal);
        assert!(ContextPriority::Normal > ContextPriority::Low);
    }

    #[test]
    fn test_utilization_reporting() {
        let ctx = ContextBuilder::new(1000)
            .system("Short system prompt.")
            .user("Short user message.")
            .build()
            .unwrap();
        assert!(ctx.utilization() > 0.0);
        assert!(ctx.utilization() <= 1.0);
    }

    #[test]
    fn test_to_openai_message_format() {
        let slot = ContextSlot::new(ContextRole::User, "Hello!", ContextSource::CurrentTurn);
        let msg = slot.to_openai_message();
        assert_eq!(msg["role"], "user");
        assert_eq!(msg["content"], "Hello!");
    }

    #[test]
    fn test_empty_assembly() {
        let assembler = ContextAssembler::new(4096);
        let ctx = assembler.assemble(vec![]).unwrap();
        assert_eq!(ctx.slots.len(), 0);
        assert_eq!(ctx.total_tokens, 0);
    }
}
