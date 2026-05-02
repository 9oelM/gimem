# gimem

A GitHub Issues-backed memory system for AI agents. Each memory is a GitHub Issue. Retrieval uses GitHub's hybrid search API. No external database, no vector store, no infrastructure to run.

## Why GitHub Issues

- **Semantic search** -- GitHub's hybrid search surfaces memories by meaning, not just keywords.
- **Free and permanent** -- Issues are free, have no storage limits, and never expire.
- **Portable** -- Memory lives in a repository you own. Transfer it, share it, fork it.
- **Generous rate limits** -- Standard GitHub API: 5,000 requests/hour. Semantic search: 10 requests/minute.

## Memory types

| Type | Purpose | Search tier |
|------|---------|-------------|
| `episodic` | A specific event or interaction ("User asked about deployment at 3pm") | Warm (hybrid) |
| `semantic` | A general fact or preference ("User prefers Python over Java") | Cold (hybrid) |
| `procedural` | A learned skill or procedure ("To deploy: run `make prod`") | Cold (hybrid) |
| `working` | Active context for the current task; always loaded every turn | Hot (lexical, 5-min TTL) |

## Repository structure

```
memory-store/           Rust library crate
  src/
    manager.rs          MemoryManager -- the main public API
    store.rs            GitHub Issues read/write (create, get, archive, delete)
    search.rs           GitHub hybrid/semantic search with rate limiting
    consolidation.rs    Episodic consolidation pipeline and eviction
    models.rs           MemoryEntry, MemoryType, MemoryTier, builders
    labels.rs           Label definitions and bootstrap
    schema.rs           Issue body serialization (TOML front-matter)
    error.rs            MemoryError, Result
    bin/
      gimem.rs          CLI binary
  tests/
    integration.rs      Library integration tests (real GitHub API)
    cli.rs              CLI subprocess tests (real GitHub API + no-API exit code tests)
  examples/
    basic.rs            Full lifecycle walkthrough
```

## Quickstart: Rust library

```toml
[dependencies]
memory-store = { git = "https://github.com/9oelM/gimem", branch = "main" }
tokio = { version = "1", features = ["full"] }
```

```rust
use memory_store::{MemoryManager, MemoryType};

#[tokio::main]
async fn main() {
    let mem = MemoryManager::new("owner/agent-memory", "ghp_...", None);

    // Run once per repository to create required labels.
    mem.bootstrap().await.unwrap();

    // Store a memory.
    mem.remember(
        "User prefers Rust for systems programming",
        MemoryType::Semantic,
        "alice",
        0.9,              // importance [0.0, 1.0]
        vec!["Rust".into()],
        vec!["preferences".into()],
    ).await.unwrap();

    // Retrieve relevant memories within a token budget.
    let context = mem.recall("programming language preferences", "alice", 2000).await.unwrap();
    println!("{context}");
}
```

### Full API

```rust
let mem = MemoryManager::new(repo, token, summarize_fn);

mem.bootstrap().await?;
mem.remember(content, type, user, importance, entities, tags).await?;
mem.recall(query, user, token_budget).await?;   // returns a formatted context string
mem.forget(issue_number).await?;                // hard delete
mem.set_working(content, user).await?;          // replace working memory
mem.clear_working(user).await?;                 // archive working memory
mem.start_session(user, description).await?;    // returns session_id
mem.end_session(user, session_id).await?;       // consolidate + close session
mem.consolidate(user).await?;                   // batch episodic consolidation
mem.evict(user, dry_run).await?;                // archive low-retention entries
```

### Custom summarisation

`MemoryManager::new` accepts an optional `SummarizeFn` called when consolidating episodic memories into semantic ones. Pass `None` for the built-in stub, or inject an LLM call:

```rust
use memory_store::SummarizeFn;
use std::sync::Arc;

let summarize: SummarizeFn = Arc::new(|contents| Box::pin(async move {
    my_llm_summarize(contents).await
}));

let mem = MemoryManager::new("owner/repo", "ghp_...", Some(summarize));
```

## Quickstart: CLI

### Install

```bash
cargo install --git https://github.com/9oelM/gimem --branch main memory-store --bin gimem
```

Or clone and build locally:

```bash
git clone https://github.com/9oelM/gimem
cargo install --path gimem/memory-store --bin gimem
```

### Required environment variables

```bash
export GITHUB_TOKEN=ghp_...          # PAT with repo scope
export GIMEM_REPO=myorg/agent-memory # repository to store memories in
export GIMEM_USER=myusername         # user identifier for filtering
```

All three can also be passed as flags (`--token`, `--repo`, `--user`).

### One-time bootstrap

```bash
gimem bootstrap
```

### Core commands

```bash
# Store memories
gimem remember "User prefers short responses" --type semantic
gimem remember "User asked about OAuth2 at 2pm" --type episodic
gimem remember "Deploy: pnpm build, rsync dist/, restart nginx" --type procedural

# Retrieve context before responding
gimem recall "user preferences" --budget 4000

# Working memory (current task)
gimem set-working "Implementing the auth flow, currently on token refresh"
gimem clear-working

# Session lifecycle
SESSION=$(gimem start-session "OAuth2 implementation" --json | jq -r '.session_id')
# ...do work, store memories...
gimem end-session "$SESSION"

# Maintenance
gimem consolidate          # batch-promote episodics to semantics
gimem evict --dry-run      # preview what would be archived
gimem evict                # archive low-retention memories
gimem forget 42            # hard-delete issue #42
```

### JSON output

Add `--json` to any command for machine-readable output.

```bash
gimem recall "deployment process" --budget 2000 --json
# {"context": "..."}

gimem remember "User prefers tabs" --type semantic --json
# {"issue_number": 55, "content": "...", "memory_type": "semantic"}

gimem start-session "Feature work" --json
# {"session_id": "78"}

gimem evict --dry-run --json
# {"evicted": 0, "candidates": 3, "dry_run": true}
```

### All flags

| Flag | Env var | Description |
|------|---------|-------------|
| `--token` | `GITHUB_TOKEN` | PAT with `repo` scope |
| `--repo` | `GIMEM_REPO` | Repository in `owner/repo` format |
| `--user` | `GIMEM_USER` | User identifier |
| `--json` | -- | Machine-readable JSON output |

## Agent workflow

For Claude/Codex, see [SKILL.md](SKILL.md). Short version:

1. At conversation start: `gimem recall "<user message>" --budget 4000`
2. When the user states a preference or decision: `gimem remember "<content>" --type semantic`
3. When starting a task: `gimem set-working "<current task>"`
4. At end of session: `gimem end-session "$SESSION"` to consolidate

## Tests

```bash
cd memory-store

# Unit tests (no network)
cargo test --lib
cargo test --bin gimem

# CLI exit-code tests (no network)
cargo test --test cli missing

# Full integration tests (requires GITHUB_TOKEN + MEMORY_TEST_REPO)
cargo test --test integration
cargo test --test cli
```

Integration tests skip cleanly when env vars are absent. All test-created issues are tagged `test:cleanup` and closed after each test.

## CI

GitHub Actions runs on every push and pull request:

- `cargo fmt --check`
- `cargo clippy --all-targets` with `-D warnings`
- Unit tests
- Binary build
- Integration tests (requires `MEMORY_GITHUB_TOKEN` and `MEMORY_TEST_REPO` secrets)

## License

MIT
