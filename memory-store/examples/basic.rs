//! Basic usage example for memory-store.
//!
//! Demonstrates the full memory lifecycle:
//!   bootstrap → remember → recall → session → consolidate → evict
//!
//! # Running
//!
//! ```bash
//! GITHUB_TOKEN=ghp_... MEMORY_TEST_REPO=owner/repo cargo run --example basic
//! ```

use memory_store::{MemoryManager, MemoryType};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let token = std::env::var("GITHUB_TOKEN").expect("GITHUB_TOKEN required");
    let repo = std::env::var("MEMORY_TEST_REPO").expect("MEMORY_TEST_REPO required");

    println!("=== memory-store basic example ===");
    println!("Repository: {repo}");

    // -------------------------------------------------------------------------
    // 1. Bootstrap — ensure all required labels exist (idempotent)
    // -------------------------------------------------------------------------
    println!("\n[1/6] Bootstrapping repo labels...");
    let mem = MemoryManager::new(&repo, &token, None);
    mem.bootstrap().await?;
    println!("      Labels ready.");

    // -------------------------------------------------------------------------
    // 2. Start a session (creates a GitHub milestone)
    // -------------------------------------------------------------------------
    println!("\n[2/6] Starting session...");
    let session_id = mem
        .start_session("demo_user", "Basic example session")
        .await?;
    println!("      Session ID: {session_id}");

    // -------------------------------------------------------------------------
    // 3. Store memories of different types
    // -------------------------------------------------------------------------
    println!("\n[3/6] Storing memories...");

    let semantic = mem
        .remember(
            "User prefers Rust for systems programming. Cited memory safety and performance.",
            MemoryType::Semantic,
            "demo_user",
            0.9,
            vec!["Rust".into()],
            vec!["preferences".into()],
        )
        .await?;
    println!(
        "      Semantic memory #{} created.",
        semantic.issue_number.unwrap_or(0)
    );

    let episodic1 = mem
        .remember(
            "User asked how to implement a binary search tree at 2pm.",
            MemoryType::Episodic,
            "demo_user",
            0.5,
            vec!["binary search tree".into()],
            vec![],
        )
        .await?;
    println!(
        "      Episodic memory #{} created.",
        episodic1.issue_number.unwrap_or(0)
    );

    let episodic2 = mem
        .remember(
            "User asked about AVL tree rotations and balance factors at 3pm.",
            MemoryType::Episodic,
            "demo_user",
            0.5,
            vec!["AVL tree".into()],
            vec![],
        )
        .await?;
    println!(
        "      Episodic memory #{} created.",
        episodic2.issue_number.unwrap_or(0)
    );

    let episodic3 = mem
        .remember(
            "User asked how to rebalance a red-black tree after insertion at 4pm.",
            MemoryType::Episodic,
            "demo_user",
            0.5,
            vec!["red-black tree".into()],
            vec![],
        )
        .await?;
    println!(
        "      Episodic memory #{} created.",
        episodic3.issue_number.unwrap_or(0)
    );

    mem.set_working(
        "Currently discussing data structures and algorithms.",
        "demo_user",
    )
    .await?;
    println!("      Working memory set.");

    // -------------------------------------------------------------------------
    // 4. Recall — retrieve relevant memories for a query
    // -------------------------------------------------------------------------
    println!("\n[4/6] Recalling memories...");
    println!("      Waiting for GitHub search index to update (~5s)...");
    tokio::time::sleep(std::time::Duration::from_secs(5)).await;

    let context = mem
        .recall(
            "What programming language does the user prefer?",
            "demo_user",
            2000,
        )
        .await?;
    if context.is_empty() {
        println!("      (no results yet — search index may not have caught up)");
    } else {
        println!("      Context:\n---\n{context}\n---");
    }

    // -------------------------------------------------------------------------
    // 5. End session (runs consolidation + closes the milestone)
    // -------------------------------------------------------------------------
    println!("\n[5/6] Ending session (triggers consolidation)...");
    let stats = mem.end_session("demo_user", &session_id).await?;
    println!(
        "      Consolidation: {} episodics merged, {} semantics created",
        stats.consolidated, stats.promoted
    );

    // -------------------------------------------------------------------------
    // 6. Eviction dry run
    // -------------------------------------------------------------------------
    println!("\n[6/6] Evicting stale memories (dry run)...");
    let evict_stats = mem.evict("demo_user", true).await?;
    println!(
        "      Would evict {}/{} candidates",
        evict_stats.evicted, evict_stats.candidates
    );

    // -------------------------------------------------------------------------
    // Cleanup — archive example issues so the repo stays tidy
    // -------------------------------------------------------------------------
    println!("\nCleaning up example issues...");
    use memory_store::store::{GitHubIssuesStore, MemoryStore};
    let store = GitHubIssuesStore::new(&repo, &token);

    for n in [
        semantic.issue_number,
        episodic1.issue_number,
        episodic2.issue_number,
        episodic3.issue_number,
    ]
    .into_iter()
    .flatten()
    {
        let _ = store.archive(n, "example cleanup").await;
    }

    let _ = mem.clear_working("demo_user").await;

    println!("Example complete.");
    Ok(())
}
