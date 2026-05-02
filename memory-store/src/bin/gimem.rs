//! `gimem` — CLI wrapper for the `memory-store` library.
//!
//! All operations are thin shims over [`memory_store::MemoryManager`].  The
//! binary supports both human-readable and machine-readable (`--json`) output
//! modes and maps library errors to the exit codes described in the spec.

use std::process;

use clap::{Parser, Subcommand};
use memory_store::{MemoryError, MemoryManager, MemoryType};

// ---------------------------------------------------------------------------
// CLI definition
// ---------------------------------------------------------------------------

#[derive(Parser)]
#[command(
    name = "gimem",
    about = "GitHub Issues-backed agent memory CLI",
    version
)]
struct Cli {
    /// GitHub personal access token (falls back to GITHUB_TOKEN env var).
    #[arg(long, env = "GITHUB_TOKEN", global = true)]
    token: Option<String>,

    /// Target repository in `owner/repo` form (falls back to GIMEM_REPO env var).
    #[arg(long, env = "GIMEM_REPO", global = true)]
    repo: Option<String>,

    /// User identifier (falls back to GIMEM_USER env var).
    #[arg(long, env = "GIMEM_USER", global = true)]
    user: Option<String>,

    /// Output machine-readable JSON instead of human-readable text.
    #[arg(long, global = true)]
    json: bool,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Create all required GitHub labels (idempotent).
    Bootstrap,

    /// Store a new memory entry.
    Remember {
        /// The text content to remember.
        content: String,

        /// Memory type.
        #[arg(long, default_value = "episodic")]
        r#type: String,

        /// Subjective importance in the range [0.0, 1.0].
        #[arg(long, default_value_t = 0.5)]
        importance: f32,

        /// Comma-separated named entities to associate with this memory.
        #[arg(long, default_value = "")]
        entities: String,

        /// Comma-separated free-form tags.
        #[arg(long, default_value = "")]
        tags: String,
    },

    /// Retrieve a context block for the given query.
    Recall {
        /// Natural-language search query.
        query: String,

        /// Token budget for the returned context block.
        #[arg(long, default_value_t = 4000)]
        budget: usize,
    },

    /// Hard-delete a memory entry by GitHub issue number.
    Forget {
        /// GitHub issue number of the memory to delete.
        issue_number: u64,
    },

    /// Create or replace the working memory for the current user.
    SetWorking {
        /// New working-memory content.
        content: String,
    },

    /// Archive all working-memory entries for the current user.
    ClearWorking,

    /// Start a new conversation session and print the session ID.
    StartSession {
        /// Human-readable session description.
        description: String,
    },

    /// End a session, run consolidation, and close the milestone.
    EndSession {
        /// Session ID returned by `start-session`.
        session_id: String,
    },

    /// Run the full consolidation pipeline.
    Consolidate,

    /// Archive low-retention entries.
    Evict {
        /// Preview what would be evicted without making changes.
        #[arg(long)]
        dry_run: bool,
    },

    /// Extract memories from a Claude Code JSONL transcript file.
    ExtractFromTranscript {
        /// Path to a Claude Code JSONL transcript file.
        path: std::path::PathBuf,

        /// Shell command for LLM-based extraction (env: GIMEM_EXTRACTOR).
        #[arg(long, env = "GIMEM_EXTRACTOR")]
        extractor_script: Option<String>,

        /// Print what would be stored without actually storing anything.
        #[arg(long)]
        dry_run: bool,
    },
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

#[tokio::main]
async fn main() {
    let cli = Cli::parse();

    let token = match cli.token {
        Some(t) => t,
        None => {
            eprintln!("Error: GITHUB_TOKEN is required (--token or env var GITHUB_TOKEN)");
            process::exit(2);
        }
    };

    let repo = match cli.repo {
        Some(r) => r,
        None => {
            eprintln!("Error: GIMEM_REPO is required (--repo or env var GIMEM_REPO)");
            process::exit(2);
        }
    };

    // Some commands need a user ID; we resolve it lazily below.
    let user = cli.user;
    let json = cli.json;

    let mgr = MemoryManager::new(&repo, &token, None);

    let result = run(cli.command, &mgr, user, json).await;

    if let Err(e) = result {
        match &e {
            MemoryError::RateLimit { retry_after_secs } => {
                eprintln!("Rate limited — retry after {retry_after_secs}s");
            }
            _ => {
                eprintln!("Error: {e}");
            }
        }
        process::exit(1);
    }
}

// ---------------------------------------------------------------------------
// Command dispatch
// ---------------------------------------------------------------------------

async fn run(
    command: Command,
    mgr: &MemoryManager,
    user: Option<String>,
    json: bool,
) -> memory_store::Result<()> {
    match command {
        Command::Bootstrap => {
            mgr.bootstrap().await?;
            if json {
                println!("{}", serde_json::json!({"ok": true}));
            } else {
                println!("Bootstrap complete.");
            }
        }

        Command::Remember {
            content,
            r#type,
            importance,
            entities,
            tags,
        } => {
            let user_id = require_user(user)?;
            let memory_type: MemoryType = r#type.parse()?;

            let entity_list = split_csv(&entities);
            let tag_list = split_csv(&tags);

            let entry = mgr
                .remember(
                    &content,
                    memory_type,
                    &user_id,
                    importance,
                    entity_list,
                    tag_list,
                )
                .await?;

            let human_msg = format!(
                "Stored memory #{} ({}).",
                entry.issue_number.unwrap_or(0),
                entry.memory_type
            );
            print_entry(&entry, json, &human_msg);
        }

        Command::Recall { query, budget } => {
            let user_id = require_user(user)?;
            let context = mgr.recall(&query, &user_id, budget).await?;

            if json {
                println!("{}", serde_json::json!({"context": context}));
            } else {
                println!("{context}");
            }
        }

        Command::Forget { issue_number } => {
            mgr.forget(issue_number).await?;
            if json {
                println!(
                    "{}",
                    serde_json::json!({"ok": true, "issue_number": issue_number})
                );
            } else {
                println!("Deleted memory #{issue_number}.");
            }
        }

        Command::SetWorking { content } => {
            let user_id = require_user(user)?;
            let entry = mgr.set_working(&content, &user_id).await?;

            let human_msg = format!(
                "Working memory set (issue #{}).",
                entry.issue_number.unwrap_or(0)
            );
            print_entry(&entry, json, &human_msg);
        }

        Command::ClearWorking => {
            let user_id = require_user(user)?;
            mgr.clear_working(&user_id).await?;

            if json {
                println!("{}", serde_json::json!({"ok": true}));
            } else {
                println!("Working memory cleared.");
            }
        }

        Command::StartSession { description } => {
            let user_id = require_user(user)?;
            let session_id = mgr.start_session(&user_id, &description).await?;

            if json {
                println!("{}", serde_json::json!({"session_id": session_id}));
            } else {
                println!("{session_id}");
            }
        }

        Command::EndSession { session_id } => {
            let user_id = require_user(user)?;
            let stats = mgr.end_session(&user_id, &session_id).await?;
            print_consolidation_stats(&stats, json, "Session ended.");
        }

        Command::Consolidate => {
            let user_id = require_user(user)?;
            let stats = mgr.consolidate(&user_id).await?;
            print_consolidation_stats(&stats, json, "Consolidation done.");
        }

        Command::Evict { dry_run } => {
            let user_id = require_user(user)?;
            let stats = mgr.evict(&user_id, dry_run).await?;

            if json {
                println!(
                    "{}",
                    serde_json::json!({
                        "evicted": stats.evicted,
                        "candidates": stats.candidates,
                        "dry_run": dry_run,
                    })
                );
            } else {
                let prefix = if dry_run { "[dry-run] " } else { "" };
                println!(
                    "{prefix}Eviction done. evicted={} candidates={}",
                    stats.evicted, stats.candidates
                );
            }
        }

        Command::ExtractFromTranscript {
            path,
            extractor_script,
            dry_run,
        } => {
            let user_id = require_user(user)?;

            let raw = std::fs::read_to_string(&path).map_err(|e| {
                MemoryError::InvalidInput(format!(
                    "failed to read transcript {}: {e}",
                    path.display()
                ))
            })?;

            let turns = parse_transcript(&raw);

            let candidates = if let Some(script) = extractor_script {
                // Format conversation for the extractor script.
                let formatted: String = turns
                    .iter()
                    .map(|(role, text)| format!("{}: {}\n", role.to_uppercase(), text))
                    .collect();
                run_extractor_script(&script, &formatted).unwrap_or_default()
            } else {
                heuristic_extract(&turns)
            };

            let total_candidates = candidates.len();
            let mut stored: Vec<serde_json::Value> = Vec::new();
            let mut skipped: usize = 0;

            for candidate in candidates {
                let existing = mgr.recall(&candidate.content, &user_id, 500).await?;
                if !existing.is_empty() && existing.contains(candidate.content.as_str()) {
                    if !json {
                        println!(
                            "  [{}] Skipped (duplicate): {}",
                            candidate.memory_type, candidate.content
                        );
                    }
                    skipped += 1;
                    continue;
                }

                let memory_type: MemoryType = candidate
                    .memory_type
                    .parse()
                    .unwrap_or(MemoryType::Episodic);

                if !json {
                    println!(
                        "  [{}] {} (importance: {})",
                        candidate.memory_type, candidate.content, candidate.importance
                    );
                }

                if dry_run {
                    stored.push(serde_json::json!({
                        "content": candidate.content,
                        "memory_type": candidate.memory_type,
                        "importance": candidate.importance,
                        "dry_run": true,
                    }));
                } else {
                    let entry = mgr
                        .remember(
                            &candidate.content,
                            memory_type,
                            &user_id,
                            candidate.importance,
                            vec![],
                            vec![],
                        )
                        .await?;

                    stored.push(serde_json::json!({
                        "issue_number": entry.issue_number,
                        "content": candidate.content,
                        "memory_type": candidate.memory_type,
                        "importance": candidate.importance,
                    }));
                }
            }

            if json {
                println!(
                    "{}",
                    serde_json::json!({
                        "stored": stored,
                        "skipped": skipped,
                        "total_candidates": total_candidates,
                        "dry_run": dry_run,
                    })
                );
            } else {
                let action = if dry_run {
                    "Would extract"
                } else {
                    "Extracted"
                };
                println!(
                    "{action} {} memories from transcript.",
                    total_candidates - skipped
                );
            }
        }
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Return the user ID or exit with an error if it was not provided.
fn require_user(user: Option<String>) -> memory_store::Result<String> {
    user.ok_or_else(|| {
        MemoryError::InvalidInput(
            "GIMEM_USER is required for this command (--user or env var GIMEM_USER)".to_string(),
        )
    })
}

/// Split a comma-separated string into a `Vec<String>`, filtering empty parts.
fn split_csv(s: &str) -> Vec<String> {
    s.split(',')
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .map(str::to_owned)
        .collect()
}

/// Print a `MemoryEntry` result in human-readable or JSON form.
fn print_entry(entry: &memory_store::MemoryEntry, json: bool, human_msg: &str) {
    if json {
        println!(
            "{}",
            serde_json::json!({
                "issue_number": entry.issue_number,
                "content": entry.content,
                "memory_type": entry.memory_type.to_string(),
            })
        );
    } else {
        println!("{human_msg}");
    }
}

/// Print consolidation stats in human-readable or JSON form.
fn print_consolidation_stats(
    stats: &memory_store::ConsolidationStats,
    json: bool,
    human_prefix: &str,
) {
    if json {
        println!(
            "{}",
            serde_json::json!({
                "consolidated": stats.consolidated,
                "promoted": stats.promoted,
                "evicted": stats.evicted,
            })
        );
    } else {
        println!(
            "{human_prefix} consolidated={} promoted={} evicted={}",
            stats.consolidated, stats.promoted, stats.evicted
        );
    }
}

// ---------------------------------------------------------------------------
// Transcript extraction helpers
// ---------------------------------------------------------------------------

/// A candidate memory extracted from a transcript.
struct ExtractCandidate {
    content: String,
    memory_type: String,
    importance: f32,
}

/// Parse a Claude Code JSONL transcript into `(role, text)` pairs.
///
/// Lines with `type` other than `"user"` or `"assistant"` are skipped.
/// The `content` field may be a string or an array of content blocks.
fn parse_transcript(raw: &str) -> Vec<(String, String)> {
    let mut turns = Vec::new();

    for line in raw.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }

        let Ok(val): Result<serde_json::Value, _> = serde_json::from_str(line) else {
            continue;
        };

        let Some(kind) = val.get("type").and_then(|v| v.as_str()) else {
            continue;
        };

        if kind != "user" && kind != "assistant" {
            continue;
        }

        let role = kind.to_string();

        let Some(content) = val.get("message").and_then(|m| m.get("content")) else {
            continue;
        };

        let text = match content {
            serde_json::Value::String(s) => s.clone(),
            serde_json::Value::Array(arr) => arr
                .iter()
                .filter_map(|block| {
                    if block.get("type").and_then(|t| t.as_str()) == Some("text") {
                        block
                            .get("text")
                            .and_then(|t| t.as_str())
                            .map(str::to_owned)
                    } else {
                        None
                    }
                })
                .collect::<Vec<_>>()
                .join(" "),
            _ => continue,
        };

        if !text.is_empty() {
            turns.push((role, text));
        }
    }

    turns
}

/// Apply heuristic rules to user turns and return extraction candidates.
fn heuristic_extract(turns: &[(String, String)]) -> Vec<ExtractCandidate> {
    let mut candidates = Vec::new();

    for (role, text) in turns {
        if role != "user" {
            continue;
        }

        let len = text.len();
        if len < 15 || text.trim_end().ends_with('?') {
            continue;
        }

        let lower = text.to_lowercase();

        let (memory_type, importance): (&str, f32) = if lower.contains("prefer")
            || lower.contains("always")
            || lower.contains("like")
            || lower.contains("want")
            || lower.contains("love")
            || lower.contains("never")
            || lower.contains("don't")
            || lower.contains("shouldn't")
            || lower.contains("avoid")
            || lower.contains("hate")
        {
            ("semantic", 0.7)
        } else if lower.contains("we use")
            || lower.contains("our stack")
            || lower.contains("our repo")
            || lower.contains("our db")
            || lower.contains("we deploy")
        {
            ("semantic", 0.8)
        } else if lower.contains("to deploy")
            || lower.contains("to build")
            || lower.contains("to test")
            || lower.contains("to run")
            || lower.contains("steps to")
        {
            ("procedural", 0.75)
        } else if len > 30 {
            ("episodic", 0.4)
        } else {
            continue;
        };

        candidates.push(ExtractCandidate {
            content: text.clone(),
            memory_type: memory_type.to_string(),
            importance,
        });
    }

    candidates
}

/// Run an extractor script, feeding the formatted conversation on stdin,
/// and parse the JSON array it writes to stdout.
fn run_extractor_script(script: &str, text: &str) -> Option<Vec<ExtractCandidate>> {
    use std::io::Write;
    use std::process::{Command, Stdio};

    let mut child = Command::new("sh")
        .args(["-c", script])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .ok()?;

    child.stdin.take()?.write_all(text.as_bytes()).ok()?;

    let out = child.wait_with_output().ok()?;
    let arr: Vec<serde_json::Value> = serde_json::from_slice(&out.stdout).ok()?;

    let candidates = arr
        .into_iter()
        .filter_map(|v| {
            let content = v.get("content")?.as_str()?.to_owned();
            let memory_type = v
                .get("type")
                .and_then(|t| t.as_str())
                .unwrap_or("episodic")
                .to_owned();
            let importance = v.get("importance").and_then(|i| i.as_f64()).unwrap_or(0.5) as f32;
            Some(ExtractCandidate {
                content,
                memory_type,
                importance,
            })
        })
        .collect();

    Some(candidates)
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // --- split_csv ---

    #[test]
    fn split_csv_empty_string_returns_empty() {
        assert!(split_csv("").is_empty());
    }

    #[test]
    fn split_csv_single_item() {
        assert_eq!(split_csv("rust"), vec!["rust"]);
    }

    #[test]
    fn split_csv_multiple_items() {
        assert_eq!(split_csv("rust,go,python"), vec!["rust", "go", "python"]);
    }

    #[test]
    fn split_csv_trims_whitespace() {
        assert_eq!(
            split_csv(" rust , go , python "),
            vec!["rust", "go", "python"]
        );
    }

    #[test]
    fn split_csv_filters_empty_parts_from_trailing_comma() {
        assert_eq!(split_csv("rust,go,"), vec!["rust", "go"]);
    }

    #[test]
    fn split_csv_filters_empty_parts_from_leading_comma() {
        assert_eq!(split_csv(",rust,go"), vec!["rust", "go"]);
    }

    #[test]
    fn split_csv_only_commas_returns_empty() {
        assert!(split_csv(",,,").is_empty());
    }

    // --- require_user ---

    #[test]
    fn require_user_some_returns_the_value() {
        let result = require_user(Some("alice".to_string()));
        assert_eq!(result.unwrap(), "alice");
    }

    #[test]
    fn require_user_none_returns_invalid_input_error() {
        let err = require_user(None).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("GIMEM_USER"),
            "error message should mention GIMEM_USER, got: {msg}"
        );
    }

    // --- parse_transcript ---

    #[test]
    fn parse_transcript_skips_non_user_assistant_types() {
        let raw = r#"{"type":"file-history-snapshot","data":{}}"#;
        assert!(parse_transcript(raw).is_empty());
    }

    #[test]
    fn parse_transcript_handles_string_content() {
        let raw = r#"{"type":"user","message":{"role":"user","content":"hello world"}}"#;
        let turns = parse_transcript(raw);
        assert_eq!(turns.len(), 1);
        assert_eq!(turns[0].0, "user");
        assert_eq!(turns[0].1, "hello world");
    }

    #[test]
    fn parse_transcript_handles_array_content() {
        let raw = r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"hi there"}]}}"#;
        let turns = parse_transcript(raw);
        assert_eq!(turns.len(), 1);
        assert_eq!(turns[0].0, "assistant");
        assert_eq!(turns[0].1, "hi there");
    }

    #[test]
    fn parse_transcript_skips_non_text_blocks_in_array() {
        let raw = r#"{"type":"user","message":{"role":"user","content":[{"type":"tool_use","id":"x"},{"type":"text","text":"actual text"}]}}"#;
        let turns = parse_transcript(raw);
        assert_eq!(turns.len(), 1);
        assert_eq!(turns[0].1, "actual text");
    }

    // --- heuristic_extract ---

    #[test]
    fn heuristic_extract_skips_short_messages() {
        let turns = vec![("user".to_string(), "short".to_string())];
        assert!(heuristic_extract(&turns).is_empty());
    }

    #[test]
    fn heuristic_extract_skips_questions() {
        let turns = vec![(
            "user".to_string(),
            "What is the best way to do this?".to_string(),
        )];
        assert!(heuristic_extract(&turns).is_empty());
    }

    #[test]
    fn heuristic_extract_skips_assistant_turns() {
        let turns = vec![(
            "assistant".to_string(),
            "I always prefer tabs over spaces in my config".to_string(),
        )];
        assert!(heuristic_extract(&turns).is_empty());
    }

    #[test]
    fn heuristic_extract_detects_semantic_prefer() {
        let turns = vec![(
            "user".to_string(),
            "I prefer tabs over spaces for indentation".to_string(),
        )];
        let candidates = heuristic_extract(&turns);
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].memory_type, "semantic");
        assert!((candidates[0].importance - 0.7).abs() < f32::EPSILON);
    }

    #[test]
    fn heuristic_extract_detects_semantic_team_stack() {
        let turns = vec![(
            "user".to_string(),
            "We use pnpm for package management in our repo".to_string(),
        )];
        let candidates = heuristic_extract(&turns);
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].memory_type, "semantic");
        assert!((candidates[0].importance - 0.8).abs() < f32::EPSILON);
    }

    #[test]
    fn heuristic_extract_detects_procedural_steps() {
        let turns = vec![(
            "user".to_string(),
            "Here are the steps to deploy the service to production".to_string(),
        )];
        let candidates = heuristic_extract(&turns);
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].memory_type, "procedural");
        assert!((candidates[0].importance - 0.75).abs() < f32::EPSILON);
    }

    #[test]
    fn heuristic_extract_detects_episodic_long_message() {
        let turns = vec![(
            "user".to_string(),
            "This morning we had a long discussion about the architecture".to_string(),
        )];
        let candidates = heuristic_extract(&turns);
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].memory_type, "episodic");
        assert!((candidates[0].importance - 0.4).abs() < f32::EPSILON);
    }
}
