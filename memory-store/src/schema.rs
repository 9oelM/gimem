//! Serialisation and deserialisation between [`MemoryEntry`] and GitHub Issue body text.
//!
//! # Format
//!
//! ```text
//! {natural language content}
//!
//! **Entities:** Alice, Python
//! **Tags:** preferences
//!
//! <!-- MEMORY_META
//! {json}
//! -->
//! ```
//!
//! The HTML comment is invisible in GitHub's rendered view, but is trivially
//! parseable with a regex. It carries all structured metadata while keeping
//! the natural-language content front-loaded for semantic indexing.

use std::sync::OnceLock;

use chrono::{DateTime, Utc};
use regex::Regex;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{
    error::{MemoryError, Result},
    models::{MemoryEntry, MemoryType},
};

// ---------------------------------------------------------------------------
// Compiled regex — compiled once, reused forever
// ---------------------------------------------------------------------------

fn meta_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"(?s)<!-- MEMORY_META\n(.*?)\n-->").expect("meta regex is valid")
    })
}

fn type_prefix_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"^\[([a-z]+)\]\s*").expect("type prefix regex is valid")
    })
}

// ---------------------------------------------------------------------------
// MemoryMeta — the JSON blob inside the HTML comment
// ---------------------------------------------------------------------------

/// Structured metadata stored in the HTML comment of a GitHub Issue body.
///
/// This mirrors the metadata fields of [`MemoryEntry`] but omits `content`
/// (which lives in the visible body) and `issue_number` (which is a GitHub
/// property, not stored in the body).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryMeta {
    /// Unique identifier for this memory entry.
    pub memory_id: Uuid,
    /// The cognitive type of this memory.
    #[serde(rename = "type")]
    pub memory_type: MemoryType,
    /// Importance score (0.0–1.0).
    pub importance: f32,
    /// Confidence score (0.0–1.0).
    pub confidence: f32,
    /// User this memory belongs to.
    pub user_id: Option<String>,
    /// Agent that created this memory.
    pub agent_id: Option<String>,
    /// Session this memory was created in.
    pub session_id: Option<String>,
    /// Number of times this memory has been accessed.
    pub access_count: u32,
    /// When this memory was last accessed.
    pub last_accessed: Option<DateTime<Utc>>,
    /// When this memory was created.
    pub created_at: DateTime<Utc>,
    /// Named entities referenced in this memory.
    pub entities: Vec<String>,
    /// Categorical tags for this memory.
    pub tags: Vec<String>,
    /// Issue numbers this memory supersedes.
    pub supersedes: Vec<u64>,
    /// Issue numbers related to this memory.
    pub related_to: Vec<u64>,
    /// Arbitrary structured data.
    pub structured_data: serde_json::Value,
}

// ---------------------------------------------------------------------------
// Public functions
// ---------------------------------------------------------------------------

/// Formats the GitHub Issue title for a [`MemoryEntry`].
///
/// Format: `"[{type}] {first line of content, max 100 chars}"`
pub fn format_title(entry: &MemoryEntry) -> String {
    let first_line = entry.content.lines().next().unwrap_or("").trim();
    let summary: String = first_line.chars().take(100).collect();
    format!("[{}] {}", entry.memory_type, summary)
}

/// Formats the GitHub Issue body for a [`MemoryEntry`].
///
/// Structure:
/// 1. Natural-language content
/// 2. Blank line
/// 3. `**Entities:** …`
/// 4. `**Tags:** …`
/// 5. HTML comment block with JSON metadata
pub fn format_body(entry: &MemoryEntry) -> String {
    let entities_line = format_header_line("**Entities:**", &entry.entities);
    let tags_line = format_header_line("**Tags:**", &entry.tags);

    let meta = MemoryMeta {
        memory_id: entry.memory_id,
        memory_type: entry.memory_type.clone(),
        importance: entry.importance,
        confidence: entry.confidence,
        user_id: entry.user_id.clone(),
        agent_id: entry.agent_id.clone(),
        session_id: entry.session_id.clone(),
        access_count: entry.access_count,
        last_accessed: entry.last_accessed,
        created_at: entry.created_at,
        entities: entry.entities.clone(),
        tags: entry.tags.clone(),
        supersedes: entry.supersedes.clone(),
        related_to: entry.related_to.clone(),
        structured_data: entry.structured_data.clone(),
    };

    let json = serde_json::to_string_pretty(&meta).expect("MemoryMeta always serialises");

    format!(
        "{}\n\n{}\n{}\n\n<!-- MEMORY_META\n{}\n-->",
        entry.content, entities_line, tags_line, json
    )
}

/// Parses a GitHub Issue `_title` and `body` back into a [`MemoryEntry`].
///
/// The `_title` parameter is accepted for API symmetry with [`format_title`] but
/// the authoritative type and content come from the body. Callers should pass
/// the issue title for forward-compatibility with future fallback logic.
///
/// # Errors
///
/// - [`MemoryError::InvalidInput`] if no `MEMORY_META` block is found.
/// - [`MemoryError::Parse`] if the JSON inside the block is malformed.
pub fn parse_body(_title: &str, body: &str) -> Result<MemoryEntry> {
    let caps = meta_regex()
        .captures(body)
        .ok_or_else(|| MemoryError::InvalidInput("no MEMORY_META block found".to_owned()))?;

    let json_str = caps.get(1).map_or("", |m| m.as_str());
    let meta: MemoryMeta = serde_json::from_str(json_str)?;

    // Content is everything before the HTML comment block, trimmed.
    // caps.get(0) is the full match; its start() is the comment's position.
    let comment_start = caps.get(0).map_or(body.len(), |m| m.start());
    let raw_content = body[..comment_start].trim();

    // Strip the trailing **Entities:** and **Tags:** helper lines.
    let content = strip_helper_lines(raw_content);

    Ok(MemoryEntry {
        memory_id: meta.memory_id,
        issue_number: None,
        content,
        memory_type: meta.memory_type,
        user_id: meta.user_id,
        agent_id: meta.agent_id,
        session_id: meta.session_id,
        importance: meta.importance,
        confidence: meta.confidence,
        access_count: meta.access_count,
        last_accessed: meta.last_accessed,
        created_at: meta.created_at,
        entities: meta.entities,
        tags: meta.tags,
        structured_data: meta.structured_data,
        supersedes: meta.supersedes,
        related_to: meta.related_to,
    })
}

/// Parses the `[type]` prefix from an issue title, returning `None` if absent.
///
/// Examples:
/// - `"[semantic] User prefers Python"` → `Some(MemoryType::Semantic)`
/// - `"no bracket"` → `None`
pub fn parse_type_from_title(title: &str) -> Option<MemoryType> {
    let caps = type_prefix_regex().captures(title)?;
    let type_str = caps.get(1)?.as_str();
    type_str.parse().ok()
}

// ---------------------------------------------------------------------------
// Private helpers
// ---------------------------------------------------------------------------

/// Formats a single bold header line with a comma-joined item list.
///
/// Returns `"**Header:**"` when `items` is empty, or `"**Header:** a, b, c"` otherwise.
fn format_header_line(header: &str, items: &[String]) -> String {
    if items.is_empty() {
        header.to_owned()
    } else {
        format!("{} {}", header, items.join(", "))
    }
}

/// Removes trailing `**Entities:**` and `**Tags:**` lines (and blank lines
/// between them and the real content) from the raw content block.
fn strip_helper_lines(raw: &str) -> String {
    let lines: Vec<&str> = raw.lines().collect();

    let mut end = lines.len();
    while end > 0 {
        let line = lines[end - 1].trim();
        if line.is_empty()
            || line.starts_with("**Entities:**")
            || line.starts_with("**Tags:**")
        {
            end -= 1;
        } else {
            break;
        }
    }

    lines[..end].join("\n")
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use uuid::Uuid;

    fn sample_entry() -> MemoryEntry {
        MemoryEntry {
            memory_id: Uuid::parse_str("550e8400-e29b-41d4-a716-446655440000").unwrap(),
            issue_number: None,
            content: "User strongly prefers Python for all backend services.".to_owned(),
            memory_type: MemoryType::Semantic,
            user_id: Some("alice".to_owned()),
            agent_id: Some("agent_01".to_owned()),
            session_id: Some("sess_abc123".to_owned()),
            importance: 0.9,
            confidence: 0.95,
            access_count: 4,
            last_accessed: Some(Utc::now()),
            created_at: Utc::now(),
            entities: vec!["Alice".to_owned(), "Python".to_owned()],
            tags: vec!["preferences".to_owned(), "language".to_owned()],
            structured_data: serde_json::json!({}),
            supersedes: vec![12, 17],
            related_to: vec![8],
        }
    }

    // --- round-trip ---

    #[test]
    fn round_trip_recovers_all_fields() {
        let entry = sample_entry();
        let title = format_title(&entry);
        let body = format_body(&entry);
        let recovered = parse_body(&title, &body).expect("parse_body should succeed");

        assert_eq!(recovered.memory_id, entry.memory_id);
        assert_eq!(recovered.memory_type, entry.memory_type);
        assert_eq!(recovered.content, entry.content);
        assert_eq!(recovered.user_id, entry.user_id);
        assert_eq!(recovered.agent_id, entry.agent_id);
        assert_eq!(recovered.session_id, entry.session_id);
        assert!((recovered.importance - entry.importance).abs() < 1e-5);
        assert!((recovered.confidence - entry.confidence).abs() < 1e-5);
        assert_eq!(recovered.access_count, entry.access_count);
        assert_eq!(recovered.entities, entry.entities);
        assert_eq!(recovered.tags, entry.tags);
        assert_eq!(recovered.supersedes, entry.supersedes);
        assert_eq!(recovered.related_to, entry.related_to);
    }

    // --- format_title ---

    #[test]
    fn format_title_episodic() {
        let mut entry = sample_entry();
        entry.memory_type = MemoryType::Episodic;
        entry.content = "User asked about deployment".to_owned();
        assert!(format_title(&entry).starts_with("[episodic] "));
    }

    #[test]
    fn format_title_semantic() {
        let entry = sample_entry();
        assert!(format_title(&entry).starts_with("[semantic] "));
    }

    #[test]
    fn format_title_procedural() {
        let mut entry = sample_entry();
        entry.memory_type = MemoryType::Procedural;
        assert!(format_title(&entry).starts_with("[procedural] "));
    }

    #[test]
    fn format_title_working() {
        let mut entry = sample_entry();
        entry.memory_type = MemoryType::Working;
        assert!(format_title(&entry).starts_with("[working] "));
    }

    #[test]
    fn format_title_truncates_at_100_chars() {
        let mut entry = sample_entry();
        entry.content = "a".repeat(200);
        let title = format_title(&entry);
        // "[semantic] " prefix is 11 chars; content portion should be exactly 100
        assert_eq!(title.len(), "[semantic] ".len() + 100);
    }

    // --- parse_body errors ---

    #[test]
    fn parse_body_missing_meta_returns_invalid_input() {
        let result = parse_body("[semantic] title", "just some content without a meta block");
        assert!(
            matches!(result, Err(MemoryError::InvalidInput(_))),
            "expected InvalidInput, got: {result:?}"
        );
    }

    #[test]
    fn parse_body_malformed_json_returns_parse_error() {
        let body = "some content\n\n<!-- MEMORY_META\n{not valid json\n-->";
        let result = parse_body("[semantic] title", body);
        assert!(
            matches!(result, Err(MemoryError::Parse(_))),
            "expected Parse error, got: {result:?}"
        );
    }

    // --- parse_type_from_title ---

    #[test]
    fn parse_type_semantic() {
        assert_eq!(
            parse_type_from_title("[semantic] foo"),
            Some(MemoryType::Semantic)
        );
    }

    #[test]
    fn parse_type_episodic() {
        assert_eq!(
            parse_type_from_title("[episodic] event"),
            Some(MemoryType::Episodic)
        );
    }

    #[test]
    fn parse_type_procedural() {
        assert_eq!(
            parse_type_from_title("[procedural] how to deploy"),
            Some(MemoryType::Procedural)
        );
    }

    #[test]
    fn parse_type_working() {
        assert_eq!(
            parse_type_from_title("[working] current task"),
            Some(MemoryType::Working)
        );
    }

    #[test]
    fn parse_type_no_bracket_returns_none() {
        assert_eq!(parse_type_from_title("no bracket here"), None);
    }

    #[test]
    fn parse_type_unknown_type_returns_none() {
        assert_eq!(parse_type_from_title("[unknown] something"), None);
    }

    // --- special characters round-trip ---

    #[test]
    fn special_characters_survive_round_trip() {
        let mut entry = sample_entry();
        entry.content = "Content with `backticks`, <html>, &amp; and \"quotes\"".to_owned();
        let title = format_title(&entry);
        let body = format_body(&entry);
        let recovered = parse_body(&title, &body).expect("round-trip should succeed");
        assert_eq!(recovered.content, entry.content);
    }

    // --- entities and tags consistency ---

    #[test]
    fn entities_and_tags_in_body_match_json_metadata() {
        let entry = sample_entry();
        let body = format_body(&entry);

        assert!(body.contains("**Entities:** Alice, Python"), "entities line missing");
        assert!(body.contains("**Tags:** preferences, language"), "tags line missing");

        let caps = meta_regex()
            .captures(&body)
            .expect("MEMORY_META block should be present");
        let json_str = caps.get(1).unwrap().as_str();
        let meta: MemoryMeta = serde_json::from_str(json_str).expect("JSON should be valid");
        assert_eq!(meta.entities, entry.entities);
        assert_eq!(meta.tags, entry.tags);
    }
}
