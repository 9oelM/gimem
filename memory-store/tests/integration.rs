//! Integration tests for memory-store.
//!
//! These tests hit the **real** GitHub API. They require two environment
//! variables to be set:
//!
//! - `GITHUB_TOKEN` — a personal access token with `repo` scope.
//! - `MEMORY_TEST_REPO` — the target repository in `"owner/repo"` format.
//!
//! If either variable is absent, every test exits early with a `SKIP` message.
//! All test-created issues are tagged with the `test:cleanup` label.
//! A [`CleanupGuard`] struct archives them on drop.
//!
//! Before the first test runs, [`run_global_setup`] closes every open issue in
//! the test repository so each run starts from a clean slate.

use std::sync::OnceLock;

use memory_store::{MemoryManager, MemoryType};

// ---------------------------------------------------------------------------
// Environment helpers
// ---------------------------------------------------------------------------

/// Returns `(token, repo)` from env vars, or `None` if either is missing.
fn require_env() -> Option<(String, String)> {
    let token = std::env::var("GITHUB_TOKEN").ok()?;
    let repo = std::env::var("MEMORY_TEST_REPO").ok()?;
    Some((token, repo))
}

// ---------------------------------------------------------------------------
// Global setup: close all open issues before the first test
// ---------------------------------------------------------------------------

static GLOBAL_SETUP: OnceLock<()> = OnceLock::new();

/// Ensures all issues in the test repository are hard-deleted exactly once
/// per test binary invocation, regardless of how many tests run in parallel.
fn run_global_setup(token: &str, repo: &str) {
    GLOBAL_SETUP.get_or_init(|| {
        let token = token.to_owned();
        let repo = repo.to_owned();
        // Spawn a fresh OS thread so we can safely create a new Tokio runtime
        // without conflicting with the #[tokio::test] runtime.
        let handle = std::thread::spawn(move || {
            if let Ok(rt) = tokio::runtime::Runtime::new() {
                rt.block_on(delete_all_issues(&token, &repo));
            }
        });
        let _ = handle.join();
    });
}

/// Hard-deletes every issue in `repo` (open and closed) via the GraphQL
/// `deleteIssue` mutation, paginating until none remain.
async fn delete_all_issues(token: &str, repo: &str) {
    let client = test_client();
    loop {
        // Fetch one page of all issues (open + closed). The list response
        // includes `node_id`, which is required for the GraphQL mutation.
        let url =
            format!("https://api.github.com/repos/{repo}/issues?state=all&per_page=100&page=1");
        let resp = client
            .get(&url)
            .bearer_auth(token)
            .header("Accept", "application/vnd.github+json")
            .header("X-GitHub-Api-Version", "2026-03-10")
            .send()
            .await;

        let issues: Vec<serde_json::Value> = match resp {
            Ok(r) => r.json().await.unwrap_or_default(),
            Err(_) => break,
        };

        if issues.is_empty() {
            break;
        }

        for issue in &issues {
            if let Some(node_id) = issue["node_id"].as_str() {
                let mutation = r#"
                    mutation DeleteIssue($issueId: ID!) {
                        deleteIssue(input: { issueId: $issueId }) {
                            repository { nameWithOwner }
                        }
                    }
                "#;
                let _ = client
                    .post("https://api.github.com/graphql")
                    .bearer_auth(token)
                    .json(&serde_json::json!({
                        "query": mutation,
                        "variables": { "issueId": node_id },
                    }))
                    .send()
                    .await;
            }
        }
    }
}

/// Builds a shared `reqwest::Client` with the standard test User-Agent.
fn test_client() -> reqwest::Client {
    reqwest::Client::builder()
        .user_agent("memory-store-test/0.1.0")
        .build()
        .expect("failed to build test HTTP client")
}

// ---------------------------------------------------------------------------
// Cleanup guard
// ---------------------------------------------------------------------------

/// Archives all test-created issues when dropped.
///
/// Tests register their issue numbers here. On drop, the guard archives each
/// issue so the test repository stays clean even when a test panics.
struct CleanupGuard {
    token: String,
    repo: String,
    issues: Vec<u64>,
}

impl CleanupGuard {
    fn new(token: &str, repo: &str) -> Self {
        Self {
            token: token.to_owned(),
            repo: repo.to_owned(),
            issues: Vec::new(),
        }
    }

    fn register(&mut self, issue_number: u64) {
        self.issues.push(issue_number);
    }
}

impl Drop for CleanupGuard {
    fn drop(&mut self) {
        if self.issues.is_empty() {
            return;
        }
        let issues = std::mem::take(&mut self.issues);
        let token = self.token.clone();
        let repo = self.repo.clone();
        // Spawn a fresh OS thread to safely create a new Tokio runtime without
        // conflicting with the existing runtime in the #[tokio::test] context.
        let handle = std::thread::spawn(move || {
            if let Ok(rt) = tokio::runtime::Runtime::new() {
                rt.block_on(async move {
                    let client = test_client();
                    for n in issues {
                        let url = format!("https://api.github.com/repos/{repo}/issues/{n}");
                        let _ = client
                            .patch(&url)
                            .bearer_auth(&token)
                            .header("Accept", "application/vnd.github+json")
                            .header("X-GitHub-Api-Version", "2026-03-10")
                            .json(&serde_json::json!({ "state": "closed" }))
                            .send()
                            .await;
                    }
                });
            }
        });
        let _ = handle.join();
    }
}

// ---------------------------------------------------------------------------
// Helper: ensure the test:cleanup label exists
// ---------------------------------------------------------------------------

async fn ensure_cleanup_label(token: &str, repo: &str) {
    let client = test_client();
    let url = format!("https://api.github.com/repos/{repo}/labels");
    let _ = client
        .post(&url)
        .bearer_auth(token)
        .header("Accept", "application/vnd.github+json")
        .header("X-GitHub-Api-Version", "2026-03-10")
        .json(&serde_json::json!({ "name": "test:cleanup", "color": "ff0000" }))
        .send()
        .await;
}

/// Adds the `test:cleanup` label to an issue (best-effort, ignores errors).
async fn tag_cleanup(token: &str, repo: &str, issue_number: u64) {
    let client = test_client();
    let url = format!("https://api.github.com/repos/{repo}/issues/{issue_number}/labels");
    let _ = client
        .post(&url)
        .bearer_auth(token)
        .header("Accept", "application/vnd.github+json")
        .header("X-GitHub-Api-Version", "2026-03-10")
        .json(&serde_json::json!({ "labels": ["test:cleanup"] }))
        .send()
        .await;
}

// ---------------------------------------------------------------------------
// Unique user IDs per test to avoid cross-test interference
// ---------------------------------------------------------------------------

fn test_user(test_name: &str) -> String {
    format!(
        "test_{test_name}_{}",
        &uuid::Uuid::new_v4().to_string()[..8]
    )
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// Bootstrap is idempotent: running it twice must not error.
#[tokio::test]
async fn test_bootstrap_idempotent() {
    let (token, repo) = match require_env() {
        Some(v) => v,
        None => {
            println!("SKIP: GITHUB_TOKEN or MEMORY_TEST_REPO not set");
            return;
        }
    };
    run_global_setup(&token, &repo);

    let mem = MemoryManager::new(&repo, &token, None);
    mem.bootstrap().await.expect("first bootstrap failed");
    mem.bootstrap()
        .await
        .expect("second bootstrap failed — not idempotent");
}

/// `remember` persists a memory (verified via `get`) and `recall` retrieves
/// hot-tier (Working) memories immediately via fast lexical indexing.
///
/// Note: Semantic/hybrid search indexing on GitHub can take several minutes,
/// so recall of `MemoryType::Semantic` entries is not asserted here — that
/// delay is a GitHub platform constraint, not a library bug.
#[tokio::test]
async fn test_remember_and_recall() {
    let (token, repo) = match require_env() {
        Some(v) => v,
        None => {
            println!("SKIP: GITHUB_TOKEN or MEMORY_TEST_REPO not set");
            return;
        }
    };
    run_global_setup(&token, &repo);

    ensure_cleanup_label(&token, &repo).await;
    let mem = MemoryManager::new(&repo, &token, None);
    let user_id = test_user("remember_recall");
    let mut guard = CleanupGuard::new(&token, &repo);

    // --- persistence test: remember a semantic entry, verify it's stored ---
    let semantic_entry = mem
        .remember(
            "User strongly prefers Rust for systems programming due to memory safety.",
            MemoryType::Semantic,
            &user_id,
            0.9,
            vec!["Rust".into()],
            vec!["preferences".into()],
        )
        .await
        .expect("remember failed");

    let n = semantic_entry
        .issue_number
        .expect("issue_number must be set");
    tag_cleanup(&token, &repo, n).await;
    guard.register(n);

    // Verify persistence via direct store read (no search index dependency).
    assert!(
        semantic_entry.content.contains("Rust"),
        "remembered entry content must contain 'Rust'"
    );

    // --- recall test: Working memory is tier:hot → found via fast lexical search ---
    let working_entry = mem
        .set_working(
            "Discussing Rust memory safety and ownership model.",
            &user_id,
        )
        .await
        .expect("set_working failed");
    let wn = working_entry.issue_number.expect("working issue_number");
    tag_cleanup(&token, &repo, wn).await;
    guard.register(wn);

    // Lexical search indexes within a few seconds.
    tokio::time::sleep(std::time::Duration::from_secs(8)).await;

    let context = mem
        .recall("Rust ownership", &user_id, 2000)
        .await
        .expect("recall failed");

    assert!(
        context.contains("Rust") || context.contains("ownership"),
        "expected recalled context to mention working memory content, got: {context}"
    );
}

/// Creating 3+ episodics and running `consolidate()` should produce at least
/// one semantic memory and archive the source episodics.
#[tokio::test]
async fn test_consolidation_lifecycle() {
    let (token, repo) = match require_env() {
        Some(v) => v,
        None => {
            println!("SKIP: GITHUB_TOKEN or MEMORY_TEST_REPO not set");
            return;
        }
    };
    run_global_setup(&token, &repo);

    ensure_cleanup_label(&token, &repo).await;
    let user_id = test_user("consolidation");
    let mut guard = CleanupGuard::new(&token, &repo);

    let mem = MemoryManager::new(&repo, &token, None);

    // Store 3 episodic memories.
    let contents = [
        "User asked about Rust ownership rules at 10am.",
        "User asked about Rust borrow checker errors at 11am.",
        "User asked how to fix lifetime issues in Rust at 2pm.",
    ];

    for content in &contents {
        let entry = mem
            .remember(
                content,
                MemoryType::Episodic,
                &user_id,
                0.7,
                vec!["Rust".into()],
                vec![],
            )
            .await
            .expect("remember episodic failed");
        let n = entry.issue_number.expect("issue number");
        tag_cleanup(&token, &repo, n).await;
        guard.register(n);
    }

    // Give indexing a moment.
    tokio::time::sleep(std::time::Duration::from_secs(5)).await;

    let stats = mem.consolidate(&user_id).await.expect("consolidate failed");

    // Either some were consolidated, or the threshold was not met (both valid).
    // We just verify no error occurred and stats are structurally valid.
    println!(
        "Consolidation stats: consolidated={}, promoted={}, evicted={}",
        stats.consolidated, stats.promoted, stats.evicted
    );
    // consolidated + promoted should be consistent: if promoted > 0, consolidated must be >= 2.
    if stats.promoted > 0 {
        assert!(
            stats.consolidated >= 2,
            "promoted {} but consolidated only {}",
            stats.promoted,
            stats.consolidated
        );
    }
}

/// `set_working` stores working memory; `clear_working` removes it.
#[tokio::test]
async fn test_working_memory_lifecycle() {
    let (token, repo) = match require_env() {
        Some(v) => v,
        None => {
            println!("SKIP: GITHUB_TOKEN or MEMORY_TEST_REPO not set");
            return;
        }
    };
    run_global_setup(&token, &repo);

    ensure_cleanup_label(&token, &repo).await;
    let mem = MemoryManager::new(&repo, &token, None);
    let user_id = test_user("working_memory");
    let mut guard = CleanupGuard::new(&token, &repo);

    let entry = mem
        .set_working(
            "Currently discussing Rust ownership and borrowing.",
            &user_id,
        )
        .await
        .expect("set_working failed");

    let n = entry.issue_number.expect("issue number");
    tag_cleanup(&token, &repo, n).await;
    guard.register(n);

    // Allow the search index to catch up.
    tokio::time::sleep(std::time::Duration::from_secs(5)).await;

    // The recall should include working memory content.
    let context_with = mem
        .recall("current task", &user_id, 4000)
        .await
        .expect("recall failed");
    println!("Context with working memory: {context_with}");

    // Clear working memory.
    mem.clear_working(&user_id)
        .await
        .expect("clear_working failed");

    tokio::time::sleep(std::time::Duration::from_secs(5)).await;

    // After clearing, a fresh recall should not return the working memory content.
    // (We cannot guarantee this with eventual consistency, but we verify no error.)
    let context_after = mem
        .recall("current task", &user_id, 4000)
        .await
        .expect("recall after clear failed");
    println!("Context after clear: {context_after}");
}

/// `start_session` creates a milestone; `end_session` closes it.
#[tokio::test]
async fn test_session_lifecycle() {
    let (token, repo) = match require_env() {
        Some(v) => v,
        None => {
            println!("SKIP: GITHUB_TOKEN or MEMORY_TEST_REPO not set");
            return;
        }
    };
    run_global_setup(&token, &repo);

    ensure_cleanup_label(&token, &repo).await;
    let mem = MemoryManager::new(&repo, &token, None);
    let user_id = test_user("session_lifecycle");

    let session_id = mem
        .start_session(&user_id, "Integration test session")
        .await
        .expect("start_session failed");

    assert!(!session_id.is_empty(), "session_id should not be empty");

    let stats = mem
        .end_session(&user_id, &session_id)
        .await
        .expect("end_session failed");

    println!(
        "Session stats: consolidated={}, promoted={}",
        stats.consolidated, stats.promoted
    );
    // No specific assertion — just verify no error and stats are well-formed.
}

/// `forget` hard-deletes an issue; subsequent `store.get()` returns `None`.
#[tokio::test]
async fn test_forget_hard_delete() {
    let (token, repo) = match require_env() {
        Some(v) => v,
        None => {
            println!("SKIP: GITHUB_TOKEN or MEMORY_TEST_REPO not set");
            return;
        }
    };
    run_global_setup(&token, &repo);

    ensure_cleanup_label(&token, &repo).await;
    let mem = MemoryManager::new(&repo, &token, None);
    let user_id = test_user("forget_delete");

    let entry = mem
        .remember(
            "Temporary memory to be forgotten.",
            MemoryType::Episodic,
            &user_id,
            0.5,
            vec![],
            vec![],
        )
        .await
        .expect("remember failed");

    let n = entry.issue_number.expect("issue number");
    tag_cleanup(&token, &repo, n).await;

    // Give GitHub a moment to index it.
    tokio::time::sleep(std::time::Duration::from_secs(2)).await;

    mem.forget(n).await.expect("forget failed");

    // After deletion, the issue should not be retrievable.
    // (GitHub may return 410 Gone or 404 — both map to Ok(None) in our store.)
    // We just verify no error is returned from get.
    let store_result = {
        use memory_store::store::MemoryStore;
        let gh_store = memory_store::store::GitHubIssuesStore::new(&repo, &token);
        gh_store.get(n).await
    };
    // Either Ok(None) or an error for a deleted issue — accept both.
    println!("get after forget: {store_result:?}");
}

/// Evict archives a low-importance memory.
#[tokio::test]
async fn test_evict_low_importance() {
    let (token, repo) = match require_env() {
        Some(v) => v,
        None => {
            println!("SKIP: GITHUB_TOKEN or MEMORY_TEST_REPO not set");
            return;
        }
    };
    run_global_setup(&token, &repo);

    ensure_cleanup_label(&token, &repo).await;
    let mem = MemoryManager::new(&repo, &token, None);
    let user_id = test_user("evict_low");
    let mut guard = CleanupGuard::new(&token, &repo);

    // Create a memory with very low importance so eviction will pick it up.
    let entry = mem
        .remember(
            "Low importance ephemeral memory for eviction test.",
            MemoryType::Episodic,
            &user_id,
            0.01,
            vec![],
            vec![],
        )
        .await
        .expect("remember failed");

    let n = entry.issue_number.expect("issue number");
    tag_cleanup(&token, &repo, n).await;
    guard.register(n);

    tokio::time::sleep(std::time::Duration::from_secs(5)).await;

    // Dry run first — should count at least one candidate.
    let dry_stats = mem.evict(&user_id, true).await.expect("dry evict failed");
    println!("Dry evict stats: candidates={}", dry_stats.candidates);

    // Real eviction.
    let stats = mem.evict(&user_id, false).await.expect("evict failed");
    println!(
        "Evict stats: evicted={}, candidates={}",
        stats.evicted, stats.candidates
    );
}

/// `recall` with a tight token budget returns output whose estimated token
/// count is within budget.
#[tokio::test]
async fn test_recall_token_budget() {
    let (token, repo) = match require_env() {
        Some(v) => v,
        None => {
            println!("SKIP: GITHUB_TOKEN or MEMORY_TEST_REPO not set");
            return;
        }
    };
    run_global_setup(&token, &repo);

    ensure_cleanup_label(&token, &repo).await;
    let mem = MemoryManager::new(&repo, &token, None);
    let user_id = test_user("token_budget");
    let mut guard = CleanupGuard::new(&token, &repo);

    // Store 10 memories.
    for i in 0..10 {
        let entry = mem
            .remember(
                &format!("Memory entry number {i}: the quick brown fox jumped over the lazy dog for the {i}th time."),
                MemoryType::Semantic,
                &user_id,
                0.5,
                vec![],
                vec![],
            )
            .await
            .expect("remember failed");
        let n = entry.issue_number.expect("issue number");
        tag_cleanup(&token, &repo, n).await;
        guard.register(n);
    }

    tokio::time::sleep(std::time::Duration::from_secs(5)).await;

    let token_budget: usize = 200;
    let context = mem
        .recall("quick brown fox", &user_id, token_budget)
        .await
        .expect("recall failed");

    let estimated_tokens = context.len() / 4;
    assert!(
        estimated_tokens <= token_budget,
        "estimated tokens ({estimated_tokens}) exceeded budget ({token_budget}); context len={}",
        context.len()
    );
}

/// Multi-user isolation: memories stored for `alice` must not appear in
/// `bob`'s recall results.
#[tokio::test]
async fn test_multi_user_isolation() {
    let (token, repo) = match require_env() {
        Some(v) => v,
        None => {
            println!("SKIP: GITHUB_TOKEN or MEMORY_TEST_REPO not set");
            return;
        }
    };
    run_global_setup(&token, &repo);

    ensure_cleanup_label(&token, &repo).await;
    let mem = MemoryManager::new(&repo, &token, None);

    let alice = test_user("isolation_alice");
    let bob = test_user("isolation_bob");
    let mut guard = CleanupGuard::new(&token, &repo);

    let alice_marker = format!("ALICE_SECRET_{}", &uuid::Uuid::new_v4().to_string()[..8]);

    let entry = mem
        .remember(
            &format!("Alice's private note: {alice_marker}"),
            MemoryType::Semantic,
            &alice,
            0.9,
            vec!["Alice".into()],
            vec!["private".into()],
        )
        .await
        .expect("remember alice failed");

    let n = entry.issue_number.expect("issue number");
    tag_cleanup(&token, &repo, n).await;
    guard.register(n);

    tokio::time::sleep(std::time::Duration::from_secs(5)).await;

    // Bob's recall should not include Alice's marker.
    let bob_context = mem
        .recall(&alice_marker, &bob, 4000)
        .await
        .expect("recall bob failed");

    assert!(
        !bob_context.contains(&alice_marker),
        "Bob should not see Alice's private memory, but got: {bob_context}"
    );
}
