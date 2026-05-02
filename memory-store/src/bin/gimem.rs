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
