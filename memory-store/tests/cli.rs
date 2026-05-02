//! CLI integration tests for the `gimem` binary.
//!
//! Tests are divided into two groups:
//!
//! 1. **No-API tests** — verify exit codes and error messages without a real
//!    GitHub token. These run in every CI environment.
//!
//! 2. **GitHub API tests** — invoke the binary against a real repository.
//!    They require the same env vars as the library integration tests:
//!    - `GITHUB_TOKEN` — PAT with `repo` scope
//!    - `MEMORY_TEST_REPO` — `owner/repo` format
//!
//!    If either variable is absent the test prints `SKIP` and returns early.
//!
//! Before the first API test runs, [`run_global_setup`] closes every open
//! issue in the test repository so each run starts from a clean slate.

use std::process::Command;
use std::sync::OnceLock;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Returns the path to the compiled `gimem` binary.
fn gimem() -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_BIN_EXE_gimem"))
}

/// Returns `(token, repo)` from env vars, or `None` to skip.
fn require_env() -> Option<(String, String)> {
    let token = std::env::var("GITHUB_TOKEN").ok()?;
    let repo = std::env::var("MEMORY_TEST_REPO").ok()?;
    Some((token, repo))
}

// ---------------------------------------------------------------------------
// Global setup: close all open issues before the first test
// ---------------------------------------------------------------------------

static GLOBAL_SETUP: OnceLock<()> = OnceLock::new();

/// Ensures all open issues in the test repository are closed exactly once
/// per test binary invocation, regardless of how many tests run in parallel.
fn run_global_setup(token: &str, repo: &str) {
    GLOBAL_SETUP.get_or_init(|| {
        let token = token.to_owned();
        let repo = repo.to_owned();
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
    let client = reqwest::Client::builder()
        .user_agent("memory-store-test/0.1.0")
        .build()
        .expect("failed to build HTTP client");

    loop {
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

/// Run `gimem` with the given args in a clean environment (no inherited
/// `GITHUB_TOKEN` / `GIMEM_REPO` / `GIMEM_USER`).
fn run_clean(args: &[&str]) -> std::process::Output {
    Command::new(gimem())
        .args(args)
        .env_remove("GITHUB_TOKEN")
        .env_remove("GIMEM_REPO")
        .env_remove("GIMEM_USER")
        .output()
        .expect("failed to execute gimem")
}

/// Run `gimem` with explicit token + repo env vars and the given args.
fn run_with_creds(token: &str, repo: &str, user: &str, args: &[&str]) -> std::process::Output {
    Command::new(gimem())
        .args(args)
        .env("GITHUB_TOKEN", token)
        .env("GIMEM_REPO", repo)
        .env("GIMEM_USER", user)
        .output()
        .expect("failed to execute gimem")
}

/// Unique test user to avoid cross-test interference.
fn test_user(label: &str) -> String {
    format!("clitest_{label}_{}", &uuid::Uuid::new_v4().to_string()[..8])
}

/// Parse stdout as JSON, panicking with a descriptive message on failure.
fn parse_json(output: &std::process::Output) -> serde_json::Value {
    let stdout = String::from_utf8_lossy(&output.stdout);
    serde_json::from_str(&stdout)
        .unwrap_or_else(|e| panic!("stdout is not valid JSON: {e}\nstdout: {stdout}"))
}

// ---------------------------------------------------------------------------
// No-API tests — exit codes and error messages
// ---------------------------------------------------------------------------

#[test]
fn missing_token_exits_with_code_2() {
    let out = run_clean(&["--repo", "owner/repo", "remember", "test"]);
    assert_eq!(
        out.status.code(),
        Some(2),
        "expected exit code 2 for missing token, got {:?}\nstderr: {}",
        out.status.code(),
        String::from_utf8_lossy(&out.stderr)
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("GITHUB_TOKEN"),
        "stderr should mention GITHUB_TOKEN, got: {stderr}"
    );
}

#[test]
fn missing_repo_exits_with_code_2() {
    let out = run_clean(&["--token", "fake_token", "remember", "test"]);
    assert_eq!(
        out.status.code(),
        Some(2),
        "expected exit code 2 for missing repo, got {:?}\nstderr: {}",
        out.status.code(),
        String::from_utf8_lossy(&out.stderr)
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("GIMEM_REPO"),
        "stderr should mention GIMEM_REPO, got: {stderr}"
    );
}

/// Commands that require `--user` / `GIMEM_USER` should exit 1 when it is
/// absent (the error propagates as a library `InvalidInput`, exit 1).
#[test]
fn missing_user_for_remember_exits_with_code_1() {
    let out = Command::new(gimem())
        .args(["remember", "test"])
        .env("GITHUB_TOKEN", "fake_token")
        .env("GIMEM_REPO", "owner/repo")
        .env_remove("GIMEM_USER")
        .output()
        .expect("failed to execute gimem");

    assert_eq!(
        out.status.code(),
        Some(1),
        "expected exit code 1 for missing user, got {:?}\nstderr: {}",
        out.status.code(),
        String::from_utf8_lossy(&out.stderr)
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("GIMEM_USER"),
        "stderr should mention GIMEM_USER, got: {stderr}"
    );
}

// ---------------------------------------------------------------------------
// GitHub API tests — JSON output schema
// ---------------------------------------------------------------------------

/// Runs `gimem bootstrap --json` and checks the response shape.
#[test]
fn bootstrap_json_output() {
    let (token, repo) = match require_env() {
        Some(v) => v,
        None => {
            println!("SKIP: GITHUB_TOKEN or MEMORY_TEST_REPO not set");
            return;
        }
    };
    run_global_setup(&token, &repo);
    let user = test_user("bootstrap");
    let out = run_with_creds(&token, &repo, &user, &["bootstrap", "--json"]);
    assert!(
        out.status.success(),
        "bootstrap --json failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let json = parse_json(&out);
    assert_eq!(json["ok"], true, "expected {{ok: true}}, got {json}");
}

/// Runs `gimem remember --json` and checks the response contains the required fields.
#[test]
fn remember_json_output() {
    let (token, repo) = match require_env() {
        Some(v) => v,
        None => {
            println!("SKIP: GITHUB_TOKEN or MEMORY_TEST_REPO not set");
            return;
        }
    };
    run_global_setup(&token, &repo);
    let user = test_user("remember");
    let out = run_with_creds(
        &token,
        &repo,
        &user,
        &[
            "remember",
            "CLI test memory content",
            "--type",
            "episodic",
            "--json",
        ],
    );
    assert!(
        out.status.success(),
        "remember --json failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let json = parse_json(&out);
    assert!(
        json["issue_number"].is_number(),
        "expected numeric issue_number, got {json}"
    );
    assert!(
        json["content"].is_string(),
        "expected string content, got {json}"
    );
    assert!(
        json["memory_type"].is_string(),
        "expected string memory_type, got {json}"
    );

    // Clean up.
    if let Some(n) = json["issue_number"].as_u64() {
        run_with_creds(&token, &repo, &user, &["forget", &n.to_string()]);
    }
}

/// Runs `gimem set-working --json` then `gimem recall --json`.
#[test]
fn set_working_and_recall_json_output() {
    let (token, repo) = match require_env() {
        Some(v) => v,
        None => {
            println!("SKIP: GITHUB_TOKEN or MEMORY_TEST_REPO not set");
            return;
        }
    };
    run_global_setup(&token, &repo);
    let user = test_user("set_working_recall");
    let out = run_with_creds(
        &token,
        &repo,
        &user,
        &["set-working", "CLI working memory test", "--json"],
    );
    assert!(
        out.status.success(),
        "set-working --json failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let json = parse_json(&out);
    assert!(
        json["issue_number"].is_number(),
        "expected numeric issue_number, got {json}"
    );
    assert_eq!(
        json["memory_type"].as_str(),
        Some("working"),
        "expected memory_type == working, got {json}"
    );

    // Wait for GitHub lexical index.
    std::thread::sleep(std::time::Duration::from_secs(8));

    // Recall returns {"context": "..."}.
    let recall_out = run_with_creds(
        &token,
        &repo,
        &user,
        &["recall", "current task", "--budget", "2000", "--json"],
    );
    assert!(
        recall_out.status.success(),
        "recall --json failed: {}",
        String::from_utf8_lossy(&recall_out.stderr)
    );
    let recall_json = parse_json(&recall_out);
    assert!(
        recall_json["context"].is_string(),
        "expected string context, got {recall_json}"
    );

    // Clean up.
    if let Some(n) = json["issue_number"].as_u64() {
        run_with_creds(&token, &repo, &user, &["forget", &n.to_string()]);
    }
}

/// `clear-working --json` returns `{"ok": true}`.
#[test]
fn clear_working_json_output() {
    let (token, repo) = match require_env() {
        Some(v) => v,
        None => {
            println!("SKIP: GITHUB_TOKEN or MEMORY_TEST_REPO not set");
            return;
        }
    };
    run_global_setup(&token, &repo);
    let user = test_user("clear_working");

    // Create a working memory entry first.
    run_with_creds(
        &token,
        &repo,
        &user,
        &["set-working", "temp working memory for clear test"],
    );

    let out = run_with_creds(&token, &repo, &user, &["clear-working", "--json"]);
    assert!(
        out.status.success(),
        "clear-working --json failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let json = parse_json(&out);
    assert_eq!(json["ok"], true, "expected {{ok: true}}, got {json}");
}

/// `forget --json` returns `{"ok": true, "issue_number": N}`.
#[test]
fn forget_json_output() {
    let (token, repo) = match require_env() {
        Some(v) => v,
        None => {
            println!("SKIP: GITHUB_TOKEN or MEMORY_TEST_REPO not set");
            return;
        }
    };
    run_global_setup(&token, &repo);
    let user = test_user("forget");

    // Create an entry to forget.
    let remember_out = run_with_creds(
        &token,
        &repo,
        &user,
        &[
            "remember",
            "temporary entry to forget",
            "--type",
            "episodic",
            "--json",
        ],
    );
    assert!(
        remember_out.status.success(),
        "remember failed: {}",
        String::from_utf8_lossy(&remember_out.stderr)
    );
    let remember_json = parse_json(&remember_out);
    let issue_number = remember_json["issue_number"]
        .as_u64()
        .expect("issue_number must be a number");

    let out = run_with_creds(
        &token,
        &repo,
        &user,
        &["forget", &issue_number.to_string(), "--json"],
    );
    assert!(
        out.status.success(),
        "forget --json failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let json = parse_json(&out);
    assert_eq!(json["ok"], true, "expected ok: true, got {json}");
    assert_eq!(
        json["issue_number"].as_u64(),
        Some(issue_number),
        "expected issue_number == {issue_number}, got {json}"
    );
}

/// `start-session --json` returns `{"session_id": "..."}`;
/// `end-session --json` returns consolidation stats.
#[test]
fn start_and_end_session_json_output() {
    let (token, repo) = match require_env() {
        Some(v) => v,
        None => {
            println!("SKIP: GITHUB_TOKEN or MEMORY_TEST_REPO not set");
            return;
        }
    };
    run_global_setup(&token, &repo);
    let user = test_user("session");

    let start_out = run_with_creds(
        &token,
        &repo,
        &user,
        &["start-session", "CLI test session", "--json"],
    );
    assert!(
        start_out.status.success(),
        "start-session --json failed: {}",
        String::from_utf8_lossy(&start_out.stderr)
    );
    let start_json = parse_json(&start_out);
    let session_id = start_json["session_id"]
        .as_str()
        .expect("session_id must be a string")
        .to_string();
    assert!(!session_id.is_empty(), "session_id should not be empty");

    let end_out = run_with_creds(
        &token,
        &repo,
        &user,
        &["end-session", &session_id, "--json"],
    );
    assert!(
        end_out.status.success(),
        "end-session --json failed: {}",
        String::from_utf8_lossy(&end_out.stderr)
    );
    let end_json = parse_json(&end_out);
    assert!(
        end_json["consolidated"].is_number(),
        "expected numeric consolidated, got {end_json}"
    );
    assert!(
        end_json["promoted"].is_number(),
        "expected numeric promoted, got {end_json}"
    );
    assert!(
        end_json["evicted"].is_number(),
        "expected numeric evicted, got {end_json}"
    );
}

/// `consolidate --json` returns `{"consolidated": N, "promoted": N, "evicted": N}`.
#[test]
fn consolidate_json_output() {
    let (token, repo) = match require_env() {
        Some(v) => v,
        None => {
            println!("SKIP: GITHUB_TOKEN or MEMORY_TEST_REPO not set");
            return;
        }
    };
    run_global_setup(&token, &repo);
    let user = test_user("consolidate");

    let out = run_with_creds(&token, &repo, &user, &["consolidate", "--json"]);
    assert!(
        out.status.success(),
        "consolidate --json failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let json = parse_json(&out);
    assert!(
        json["consolidated"].is_number(),
        "expected numeric consolidated, got {json}"
    );
    assert!(
        json["promoted"].is_number(),
        "expected numeric promoted, got {json}"
    );
    assert!(
        json["evicted"].is_number(),
        "expected numeric evicted, got {json}"
    );
}

/// `evict --dry-run --json` sets `dry_run: true`; real evict sets `dry_run: false`.
#[test]
fn evict_json_output() {
    let (token, repo) = match require_env() {
        Some(v) => v,
        None => {
            println!("SKIP: GITHUB_TOKEN or MEMORY_TEST_REPO not set");
            return;
        }
    };
    run_global_setup(&token, &repo);
    let user = test_user("evict");

    // Dry-run.
    let dry_out = run_with_creds(&token, &repo, &user, &["evict", "--dry-run", "--json"]);
    assert!(
        dry_out.status.success(),
        "evict --dry-run --json failed: {}",
        String::from_utf8_lossy(&dry_out.stderr)
    );
    let dry_json = parse_json(&dry_out);
    assert_eq!(
        dry_json["dry_run"], true,
        "expected dry_run: true, got {dry_json}"
    );
    assert!(
        dry_json["candidates"].is_number(),
        "expected numeric candidates, got {dry_json}"
    );
    assert!(
        dry_json["evicted"].is_number(),
        "expected numeric evicted, got {dry_json}"
    );

    // Real evict.
    let real_out = run_with_creds(&token, &repo, &user, &["evict", "--json"]);
    assert!(
        real_out.status.success(),
        "evict --json failed: {}",
        String::from_utf8_lossy(&real_out.stderr)
    );
    let real_json = parse_json(&real_out);
    assert_eq!(
        real_json["dry_run"], false,
        "expected dry_run: false, got {real_json}"
    );
}
