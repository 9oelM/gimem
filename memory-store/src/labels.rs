//! Label taxonomy constants and constructors for GitHub Issues-backed memory.
//!
//! All memory entries are tagged with a set of structured labels that drive
//! filtering and retrieval. This module owns the canonical label strings so
//! every part of the crate uses the same values.

use crate::models::{MemoryEntry, MemoryTier, MemoryType};

// ---------------------------------------------------------------------------
// Type labels
// ---------------------------------------------------------------------------

/// Label for episodic memory entries.
pub const TYPE_EPISODIC: &str = "type:episodic";
/// Label for semantic memory entries.
pub const TYPE_SEMANTIC: &str = "type:semantic";
/// Label for procedural memory entries.
pub const TYPE_PROCEDURAL: &str = "type:procedural";
/// Label for working (hot-tier) memory entries.
pub const TYPE_WORKING: &str = "type:working";

// ---------------------------------------------------------------------------
// Tier labels
// ---------------------------------------------------------------------------

/// Label for hot-tier entries (always loaded).
pub const TIER_HOT: &str = "tier:hot";
/// Label for warm-tier entries (recent sessions).
pub const TIER_WARM: &str = "tier:warm";
/// Label for cold-tier entries (long-term consolidated).
pub const TIER_COLD: &str = "tier:cold";

// ---------------------------------------------------------------------------
// Status labels
// ---------------------------------------------------------------------------

/// Label for active (non-superseded) entries.
pub const STATUS_ACTIVE: &str = "status:active";
/// Label for entries that have been consolidated away.
pub const STATUS_SUPERSEDED: &str = "status:superseded";

// ---------------------------------------------------------------------------
// Bootstrap set — all labels that must exist in a repo before use
// ---------------------------------------------------------------------------

/// All labels that must be created on a repository before the memory system
/// can be used. Pass this slice to `MemoryManager::bootstrap`.
pub const BOOTSTRAP_LABELS: &[&str] = &[
    TYPE_EPISODIC,
    TYPE_SEMANTIC,
    TYPE_PROCEDURAL,
    TYPE_WORKING,
    TIER_HOT,
    TIER_WARM,
    TIER_COLD,
    STATUS_ACTIVE,
    STATUS_SUPERSEDED,
];

// ---------------------------------------------------------------------------
// Dynamic label constructors
// ---------------------------------------------------------------------------

/// Returns the scoping label for the given user, e.g. `"user:alice"`.
pub fn user_label(user_id: &str) -> String {
    format!("user:{user_id}")
}

/// Returns the scoping label for the given agent, e.g. `"agent:agent_01"`.
pub fn agent_label(agent_id: &str) -> String {
    format!("agent:{agent_id}")
}

/// Maps a [`MemoryType`] to its canonical label string.
pub fn type_label(t: &MemoryType) -> &'static str {
    match t {
        MemoryType::Episodic => TYPE_EPISODIC,
        MemoryType::Semantic => TYPE_SEMANTIC,
        MemoryType::Procedural => TYPE_PROCEDURAL,
        MemoryType::Working => TYPE_WORKING,
    }
}

/// Maps a [`MemoryTier`] to its canonical label string.
pub fn tier_label(t: &MemoryTier) -> &'static str {
    match t {
        MemoryTier::Hot => TIER_HOT,
        MemoryTier::Warm => TIER_WARM,
        MemoryTier::Cold => TIER_COLD,
    }
}

/// Assembles the full label set for a [`MemoryEntry`].
///
/// Always includes: type label, tier label, `status:active`.
/// Adds `user:{id}` when [`MemoryEntry::user_id`] is `Some`.
/// Adds `agent:{id}` when [`MemoryEntry::agent_id`] is `Some`.
pub fn labels_for_entry(entry: &MemoryEntry) -> Vec<String> {
    let tier = entry.tier();
    let mut labels = vec![
        type_label(&entry.memory_type).to_owned(),
        tier_label(&tier).to_owned(),
        STATUS_ACTIVE.to_owned(),
    ];
    if let Some(uid) = &entry.user_id {
        labels.push(user_label(uid));
    }
    if let Some(aid) = &entry.agent_id {
        labels.push(agent_label(aid));
    }
    labels
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{MemoryEntry, MemoryTier, MemoryType};
    use chrono::Utc;
    use uuid::Uuid;

    fn minimal_entry(memory_type: MemoryType) -> MemoryEntry {
        MemoryEntry {
            memory_id: Uuid::new_v4(),
            issue_number: None,
            content: "test content".to_owned(),
            memory_type,
            user_id: Some("alice".to_owned()),
            agent_id: None,
            session_id: None,
            importance: 0.5,
            confidence: 0.8,
            access_count: 0,
            last_accessed: None,
            created_at: Utc::now(),
            entities: vec![],
            tags: vec![],
            structured_data: serde_json::Value::Object(Default::default()),
            supersedes: vec![],
            related_to: vec![],
        }
    }

    // --- type_label ---

    #[test]
    fn type_label_episodic() {
        assert_eq!(type_label(&MemoryType::Episodic), TYPE_EPISODIC);
    }

    #[test]
    fn type_label_semantic() {
        assert_eq!(type_label(&MemoryType::Semantic), TYPE_SEMANTIC);
    }

    #[test]
    fn type_label_procedural() {
        assert_eq!(type_label(&MemoryType::Procedural), TYPE_PROCEDURAL);
    }

    #[test]
    fn type_label_working() {
        assert_eq!(type_label(&MemoryType::Working), TYPE_WORKING);
    }

    // --- tier_label ---

    #[test]
    fn tier_label_hot() {
        assert_eq!(tier_label(&MemoryTier::Hot), TIER_HOT);
    }

    #[test]
    fn tier_label_warm() {
        assert_eq!(tier_label(&MemoryTier::Warm), TIER_WARM);
    }

    #[test]
    fn tier_label_cold() {
        assert_eq!(tier_label(&MemoryTier::Cold), TIER_COLD);
    }

    // --- user_label ---

    #[test]
    fn user_label_formats_correctly() {
        assert_eq!(user_label("alice"), "user:alice");
    }

    // --- agent_label ---

    #[test]
    fn agent_label_formats_correctly() {
        assert_eq!(agent_label("bot_01"), "agent:bot_01");
    }

    // --- labels_for_entry ---

    #[test]
    fn labels_for_entry_includes_type_tier_status_user() {
        let entry = minimal_entry(MemoryType::Episodic);
        let labels = labels_for_entry(&entry);
        assert!(labels.contains(&TYPE_EPISODIC.to_owned()), "missing type label");
        assert!(labels.contains(&TIER_COLD.to_owned()), "missing tier label");
        assert!(labels.contains(&STATUS_ACTIVE.to_owned()), "missing status label");
        assert!(labels.contains(&"user:alice".to_owned()), "missing user label");
    }

    #[test]
    fn labels_for_entry_includes_agent_when_some() {
        let mut entry = minimal_entry(MemoryType::Semantic);
        entry.agent_id = Some("agent_01".to_owned());
        let labels = labels_for_entry(&entry);
        assert!(labels.contains(&"agent:agent_01".to_owned()), "missing agent label");
    }

    #[test]
    fn labels_for_entry_omits_agent_when_none() {
        let entry = minimal_entry(MemoryType::Semantic);
        assert!(entry.agent_id.is_none());
        let labels = labels_for_entry(&entry);
        let has_agent = labels.iter().any(|l| l.starts_with("agent:"));
        assert!(!has_agent, "should not have agent label when agent_id is None");
    }

    // --- BOOTSTRAP_LABELS ---

    #[test]
    fn bootstrap_labels_has_expected_count() {
        assert_eq!(BOOTSTRAP_LABELS.len(), 9);
    }
}
