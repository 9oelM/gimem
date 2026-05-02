//! Core domain types for the memory-store library.

use std::str::FromStr;
use std::fmt;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::error::{MemoryError, Result};

/// The semantic category of a memory entry.
///
/// Each type maps to a `type:*` GitHub label and influences how the entry is
/// retrieved, tiered, and consolidated.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MemoryType {
    /// A specific event or interaction (e.g. "User asked about deployment at 3pm").
    Episodic,
    /// A general fact or preference (e.g. "User prefers Python over Java").
    Semantic,
    /// A learned skill or procedure (e.g. "To deploy: run `make prod`").
    Procedural,
    /// Active context for the current task; always loaded, never searched.
    Working,
}

impl fmt::Display for MemoryType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            MemoryType::Episodic => write!(f, "episodic"),
            MemoryType::Semantic => write!(f, "semantic"),
            MemoryType::Procedural => write!(f, "procedural"),
            MemoryType::Working => write!(f, "working"),
        }
    }
}

impl FromStr for MemoryType {
    type Err = MemoryError;

    fn from_str(s: &str) -> Result<Self> {
        match s.to_lowercase().as_str() {
            "episodic" => Ok(MemoryType::Episodic),
            "semantic" => Ok(MemoryType::Semantic),
            "procedural" => Ok(MemoryType::Procedural),
            "working" => Ok(MemoryType::Working),
            other => Err(MemoryError::InvalidInput(format!(
                "unknown memory type: {other:?}; expected one of: episodic, semantic, procedural, working"
            ))),
        }
    }
}

/// The access-frequency tier of a memory entry.
///
/// Tier is *derived* from `MemoryType` and `access_count` — it is never set
/// manually.  It controls caching strategy and retrieval priority.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MemoryTier {
    /// Working memory; fetched every turn with a 5-minute local TTL.
    Hot,
    /// Short-term memory; recent sessions, 2-minute search cache.
    Warm,
    /// Long-term consolidated semantics, 2-minute search cache.
    Cold,
}

impl fmt::Display for MemoryTier {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            MemoryTier::Hot => write!(f, "hot"),
            MemoryTier::Warm => write!(f, "warm"),
            MemoryTier::Cold => write!(f, "cold"),
        }
    }
}

impl FromStr for MemoryTier {
    type Err = MemoryError;

    fn from_str(s: &str) -> Result<Self> {
        match s.to_lowercase().as_str() {
            "hot" => Ok(MemoryTier::Hot),
            "warm" => Ok(MemoryTier::Warm),
            "cold" => Ok(MemoryTier::Cold),
            other => Err(MemoryError::InvalidInput(format!(
                "unknown memory tier: {other:?}; expected one of: hot, warm, cold"
            ))),
        }
    }
}

/// A single memory entry backed by a GitHub Issue.
///
/// Each field maps either to GitHub Issue primitives (number, title, body) or
/// to JSON metadata embedded in an HTML comment inside the issue body.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryEntry {
    /// Stable client-side identifier, generated at creation time.
    pub memory_id: Uuid,

    /// GitHub issue number — `None` until the entry has been persisted.
    pub issue_number: Option<u64>,

    /// The natural language content of this memory.
    pub content: String,

    /// The semantic category of this memory.
    pub memory_type: MemoryType,

    /// Optional user identifier for multi-user scoping.
    pub user_id: Option<String>,

    /// Optional agent identifier recording which agent wrote this memory.
    pub agent_id: Option<String>,

    /// Optional session identifier linking this memory to a conversation session.
    pub session_id: Option<String>,

    /// Subjective importance of this memory, in the range `[0.0, 1.0]`.
    pub importance: f32,

    /// Confidence in the accuracy of this memory, in the range `[0.0, 1.0]`.
    pub confidence: f32,

    /// Total number of times this memory has been accessed.
    pub access_count: u32,

    /// Timestamp of the most recent access, if any.
    pub last_accessed: Option<DateTime<Utc>>,

    /// Timestamp when this memory was first created.
    pub created_at: DateTime<Utc>,

    /// Named entities extracted from or associated with this memory.
    pub entities: Vec<String>,

    /// Free-form tags for additional classification.
    pub tags: Vec<String>,

    /// Arbitrary structured JSON data attached to this memory.
    pub structured_data: serde_json::Value,

    /// Issue numbers of memories that this entry supersedes (from consolidation).
    pub supersedes: Vec<u64>,

    /// Issue numbers of memories that are conceptually related to this entry.
    pub related_to: Vec<u64>,
}

impl MemoryEntry {
    /// Derive the access-frequency [`MemoryTier`] from the entry's type and usage.
    ///
    /// Rules (applied in priority order):
    /// 1. [`MemoryType::Working`] → [`MemoryTier::Hot`]
    /// 2. [`MemoryType::Semantic`] or `access_count > 5` → [`MemoryTier::Warm`]
    /// 3. Everything else → [`MemoryTier::Cold`]
    pub fn tier(&self) -> MemoryTier {
        match self.memory_type {
            MemoryType::Working => MemoryTier::Hot,
            MemoryType::Semantic => MemoryTier::Warm,
            _ if self.access_count > 5 => MemoryTier::Warm,
            _ => MemoryTier::Cold,
        }
    }

    /// Compute a retention score indicating how valuable this memory is to keep.
    ///
    /// Formula:
    /// ```text
    /// 0.4 × importance
    /// + 0.3 × min(access_count / 10.0, 1.0)
    /// + 0.2 × max(0.0, 1.0 - age_days / 90.0)
    /// + 0.1 × confidence
    /// ```
    ///
    /// Age is measured from `created_at`.
    /// The score is in the range `[0.0, 1.0]`.
    pub fn retention_score(&self) -> f32 {
        let age_days = (Utc::now() - self.created_at).num_days() as f32;
        let access_component = (self.access_count as f32 / 10.0).min(1.0);
        let age_component = (1.0 - age_days / 90.0).max(0.0);

        0.4 * self.importance
            + 0.3 * access_component
            + 0.2 * age_component
            + 0.1 * self.confidence
    }

    /// Create a fluent builder for constructing a [`MemoryEntry`].
    pub fn builder(content: impl Into<String>, memory_type: MemoryType) -> MemoryEntryBuilder {
        MemoryEntryBuilder::new(content.into(), memory_type)
    }
}

/// Fluent builder for [`MemoryEntry`].
///
/// Required fields: `content` and `memory_type` (provided to [`MemoryEntry::builder`]).
/// All other fields have sensible defaults.
pub struct MemoryEntryBuilder {
    content: String,
    memory_type: MemoryType,
    user_id: Option<String>,
    agent_id: Option<String>,
    session_id: Option<String>,
    importance: f32,
    confidence: f32,
    entities: Vec<String>,
    tags: Vec<String>,
    structured_data: serde_json::Value,
    supersedes: Vec<u64>,
    related_to: Vec<u64>,
}

impl MemoryEntryBuilder {
    fn new(content: String, memory_type: MemoryType) -> Self {
        Self {
            content,
            memory_type,
            user_id: None,
            agent_id: None,
            session_id: None,
            importance: 0.5,
            confidence: 0.8,
            entities: Vec::new(),
            tags: Vec::new(),
            structured_data: serde_json::Value::Object(Default::default()),
            supersedes: Vec::new(),
            related_to: Vec::new(),
        }
    }

    /// Set the user identifier for multi-user scoping.
    pub fn user_id(mut self, user_id: impl Into<String>) -> Self {
        self.user_id = Some(user_id.into());
        self
    }

    /// Set the agent identifier.
    pub fn agent_id(mut self, agent_id: impl Into<String>) -> Self {
        self.agent_id = Some(agent_id.into());
        self
    }

    /// Set the session identifier.
    pub fn session_id(mut self, session_id: impl Into<String>) -> Self {
        self.session_id = Some(session_id.into());
        self
    }

    /// Set the importance score (`[0.0, 1.0]`).
    pub fn importance(mut self, importance: f32) -> Self {
        self.importance = importance;
        self
    }

    /// Set the confidence score (`[0.0, 1.0]`).
    pub fn confidence(mut self, confidence: f32) -> Self {
        self.confidence = confidence;
        self
    }

    /// Set named entities associated with this memory.
    pub fn entities(mut self, entities: Vec<String>) -> Self {
        self.entities = entities;
        self
    }

    /// Set free-form tags.
    pub fn tags(mut self, tags: Vec<String>) -> Self {
        self.tags = tags;
        self
    }

    /// Attach arbitrary structured JSON data.
    pub fn structured_data(mut self, data: serde_json::Value) -> Self {
        self.structured_data = data;
        self
    }

    /// List issue numbers that this memory supersedes.
    pub fn supersedes(mut self, supersedes: Vec<u64>) -> Self {
        self.supersedes = supersedes;
        self
    }

    /// List issue numbers that are conceptually related to this memory.
    pub fn related_to(mut self, related_to: Vec<u64>) -> Self {
        self.related_to = related_to;
        self
    }

    /// Consume the builder and produce a [`MemoryEntry`].
    ///
    /// # Errors
    ///
    /// Returns [`MemoryError::InvalidInput`] if `content` is empty or
    /// `importance` / `confidence` are outside `[0.0, 1.0]`.
    pub fn build(self) -> Result<MemoryEntry> {
        if self.content.is_empty() {
            return Err(MemoryError::InvalidInput("content must not be empty".to_string()));
        }
        if !(0.0..=1.0).contains(&self.importance) {
            return Err(MemoryError::InvalidInput(format!(
                "importance must be in [0.0, 1.0], got {}",
                self.importance
            )));
        }
        if !(0.0..=1.0).contains(&self.confidence) {
            return Err(MemoryError::InvalidInput(format!(
                "confidence must be in [0.0, 1.0], got {}",
                self.confidence
            )));
        }

        Ok(MemoryEntry {
            memory_id: Uuid::new_v4(),
            issue_number: None,
            content: self.content,
            memory_type: self.memory_type,
            user_id: self.user_id,
            agent_id: self.agent_id,
            session_id: self.session_id,
            importance: self.importance,
            confidence: self.confidence,
            access_count: 0,
            last_accessed: None,
            created_at: Utc::now(),
            entities: self.entities,
            tags: self.tags,
            structured_data: self.structured_data,
            supersedes: self.supersedes,
            related_to: self.related_to,
        })
    }
}

/// A partial update to a [`MemoryEntry`].
///
/// Only `Some` fields are applied; `None` fields are left unchanged.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MemoryPatch {
    /// Updated content, if any.
    pub content: Option<String>,
    /// Updated memory type, if any.
    pub memory_type: Option<MemoryType>,
    /// Updated user identifier, if any.
    pub user_id: Option<String>,
    /// Updated agent identifier, if any.
    pub agent_id: Option<String>,
    /// Updated session identifier, if any.
    pub session_id: Option<String>,
    /// Updated importance score, if any.
    pub importance: Option<f32>,
    /// Updated confidence score, if any.
    pub confidence: Option<f32>,
    /// Updated access count, if any.
    pub access_count: Option<u32>,
    /// Updated last-accessed timestamp, if any.
    pub last_accessed: Option<DateTime<Utc>>,
    /// Updated entities list, if any.
    pub entities: Option<Vec<String>>,
    /// Updated tags list, if any.
    pub tags: Option<Vec<String>>,
    /// Updated structured data, if any.
    pub structured_data: Option<serde_json::Value>,
    /// Updated supersedes list, if any.
    pub supersedes: Option<Vec<u64>>,
    /// Updated related_to list, if any.
    pub related_to: Option<Vec<u64>>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fresh_entry(memory_type: MemoryType) -> MemoryEntry {
        MemoryEntry::builder("test content", memory_type)
            .build()
            .expect("valid entry")
    }

    // ── tier() tests ─────────────────────────────────────────────────────────

    #[test]
    fn tier_working_is_hot() {
        let entry = fresh_entry(MemoryType::Working);
        assert_eq!(entry.tier(), MemoryTier::Hot);
    }

    #[test]
    fn tier_semantic_is_warm() {
        let entry = fresh_entry(MemoryType::Semantic);
        assert_eq!(entry.tier(), MemoryTier::Warm);
    }

    #[test]
    fn tier_high_access_count_is_warm() {
        let mut entry = fresh_entry(MemoryType::Episodic);
        entry.access_count = 6;
        assert_eq!(entry.tier(), MemoryTier::Warm);
    }

    #[test]
    fn tier_exactly_five_access_episodic_is_cold() {
        let mut entry = fresh_entry(MemoryType::Episodic);
        entry.access_count = 5; // exactly 5, not > 5
        assert_eq!(entry.tier(), MemoryTier::Cold);
    }

    #[test]
    fn tier_procedural_low_access_is_cold() {
        let entry = fresh_entry(MemoryType::Procedural);
        assert_eq!(entry.tier(), MemoryTier::Cold);
    }

    // ── retention_score() tests ───────────────────────────────────────────────

    #[test]
    fn retention_score_brand_new_high_importance() {
        // Brand-new (age≈0), high importance=1.0, confidence=1.0, 0 accesses.
        // Expected: 0.4×1.0 + 0.3×0.0 + 0.2×≈1.0 + 0.1×1.0 ≈ 0.7
        let entry = MemoryEntry::builder("test", MemoryType::Semantic)
            .importance(1.0)
            .confidence(1.0)
            .build()
            .unwrap();
        let score = entry.retention_score();
        assert!(score > 0.6, "expected high score for new important entry, got {score}");
    }

    #[test]
    fn retention_score_old_rarely_accessed_is_low() {
        // Old entry (> 90 days), zero accesses, low importance.
        let mut entry = MemoryEntry::builder("test", MemoryType::Episodic)
            .importance(0.1)
            .confidence(0.2)
            .build()
            .unwrap();
        entry.created_at = Utc::now() - chrono::Duration::days(200);
        let score = entry.retention_score();
        // 0.4×0.1 + 0.3×0.0 + 0.2×0.0 + 0.1×0.2 = 0.04 + 0.0 + 0.0 + 0.02 = 0.06
        assert!(score < 0.15, "expected low score for old neglected entry, got {score}");
    }

    #[test]
    fn retention_score_at_90_days_age_component_is_zero() {
        let mut entry = MemoryEntry::builder("test", MemoryType::Episodic)
            .importance(0.5)
            .confidence(0.8)
            .build()
            .unwrap();
        entry.created_at = Utc::now() - chrono::Duration::days(90);
        let score = entry.retention_score();
        // age_component = max(0, 1 - 90/90) = 0
        // 0.4×0.5 + 0.3×0.0 + 0.2×0.0 + 0.1×0.8 = 0.20 + 0.0 + 0.0 + 0.08 = 0.28
        let expected = 0.4 * 0.5_f32 + 0.1 * 0.8_f32;
        assert!(
            (score - expected).abs() < 0.05,
            "expected ~{expected} at 90 days, got {score}"
        );
    }

    // ── Builder tests ─────────────────────────────────────────────────────────

    #[test]
    fn builder_happy_path() {
        let entry = MemoryEntry::builder("Remember Python preference", MemoryType::Semantic)
            .user_id("alice")
            .agent_id("agent_01")
            .importance(0.9)
            .confidence(0.95)
            .entities(vec!["Python".to_string(), "Alice".to_string()])
            .tags(vec!["preferences".to_string()])
            .build()
            .unwrap();

        assert_eq!(entry.content, "Remember Python preference");
        assert_eq!(entry.memory_type, MemoryType::Semantic);
        assert_eq!(entry.user_id.as_deref(), Some("alice"));
        assert_eq!(entry.agent_id.as_deref(), Some("agent_01"));
        assert!((entry.importance - 0.9).abs() < f32::EPSILON);
        assert!((entry.confidence - 0.95).abs() < f32::EPSILON);
        assert_eq!(entry.entities, vec!["Python", "Alice"]);
        assert_eq!(entry.tags, vec!["preferences"]);
        assert_eq!(entry.access_count, 0);
        assert!(entry.issue_number.is_none());
        assert!(entry.last_accessed.is_none());
    }

    #[test]
    fn builder_defaults() {
        let entry = MemoryEntry::builder("some content", MemoryType::Episodic)
            .build()
            .unwrap();
        assert!((entry.importance - 0.5).abs() < f32::EPSILON);
        assert!((entry.confidence - 0.8).abs() < f32::EPSILON);
        assert!(entry.entities.is_empty());
        assert!(entry.tags.is_empty());
        assert!(entry.supersedes.is_empty());
        assert!(entry.related_to.is_empty());
    }

    #[test]
    fn builder_empty_content_returns_error() {
        let result = MemoryEntry::builder("", MemoryType::Episodic).build();
        assert!(matches!(result, Err(MemoryError::InvalidInput(_))));
    }

    #[test]
    fn builder_out_of_range_importance_returns_error() {
        let result = MemoryEntry::builder("test", MemoryType::Episodic)
            .importance(1.5)
            .build();
        assert!(matches!(result, Err(MemoryError::InvalidInput(_))));
    }

    #[test]
    fn builder_out_of_range_confidence_returns_error() {
        let result = MemoryEntry::builder("test", MemoryType::Episodic)
            .confidence(-0.1)
            .build();
        assert!(matches!(result, Err(MemoryError::InvalidInput(_))));
    }

    // ── MemoryType Display/FromStr round-trip ─────────────────────────────────

    #[test]
    fn memory_type_display_from_str_round_trip() {
        for variant in &[
            MemoryType::Episodic,
            MemoryType::Semantic,
            MemoryType::Procedural,
            MemoryType::Working,
        ] {
            let s = variant.to_string();
            let parsed: MemoryType = s.parse().expect("round-trip should succeed");
            assert_eq!(variant, &parsed, "round-trip failed for {variant:?}");
        }
    }

    #[test]
    fn memory_type_from_str_case_insensitive() {
        assert_eq!("EPISODIC".parse::<MemoryType>().unwrap(), MemoryType::Episodic);
        assert_eq!("Semantic".parse::<MemoryType>().unwrap(), MemoryType::Semantic);
    }

    #[test]
    fn memory_type_from_str_unknown_returns_error() {
        let result = "unknown_type".parse::<MemoryType>();
        assert!(matches!(result, Err(MemoryError::InvalidInput(_))));
    }

    #[test]
    fn memory_tier_display_from_str_round_trip() {
        for variant in &[MemoryTier::Hot, MemoryTier::Warm, MemoryTier::Cold] {
            let s = variant.to_string();
            let parsed: MemoryTier = s.parse().expect("round-trip should succeed");
            assert_eq!(variant, &parsed, "round-trip failed for {variant:?}");
        }
    }
}
