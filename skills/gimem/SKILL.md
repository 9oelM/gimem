---
name: gimem
description: Store and retrieve agent memories as GitHub Issues with semantic search via the gimem CLI. Anything you want the agent to remember, recall, or forget goes through this tool.
---

# gimem — GitHub Issues Agent Memory

## What It Is

`gimem` is a CLI tool that uses GitHub Issues as a persistent, searchable memory store for AI agents. Each memory is stored as a labeled GitHub Issue; retrieval uses GitHub's semantic/hybrid search API to surface relevant context by meaning rather than just keywords. Memory is typed (`episodic`, `semantic`, `procedural`, `working`) so agents can store raw events, distilled facts, how-to procedures, and current-task scratchpad separately — then recall exactly what is needed within a token budget.

## Setup

### Install

```bash
cargo install --git https://github.com/9oelM/gimem --branch main memory-store --bin gimem
```

Or clone and build locally:

```bash
git clone https://github.com/9oelM/gimem
cargo install --path gimem/memory-store --bin gimem
```

### Required Environment Variables

```bash
export GITHUB_TOKEN=ghp_...        # personal access token with repo scope
export GIMEM_REPO=myorg/agent-memory
export GIMEM_USER=myusername
```

Each variable can also be passed as a flag (`--token`, `--repo`, `--user`). See [Global Flags](#global-flags).

### One-Time Bootstrap

Run once per repository to create the required labels:

```bash
gimem bootstrap
```

### Automatic Extraction (Stop Hook)

Configure Claude Code to automatically extract memories at the end of every session. Add to `~/.claude/settings.json`:

```json
{
  "hooks": {
    "Stop": [
      {
        "command": "gimem extract-from-transcript \"$CLAUDE_TRANSCRIPT_PATH\" --user \"$GIMEM_USER\""
      }
    ]
  }
}
```

This runs `gimem extract-from-transcript` on the session JSONL log when Claude Code exits, capturing anything the agent missed during the conversation. The command is a no-op if `GIMEM_USER` or `GITHUB_TOKEN` is unset.

For LLM-powered extraction (higher recall), set `GIMEM_EXTRACTOR` to a shell command that reads conversation text from stdin and writes a JSON array of memories to stdout:

```bash
export GIMEM_EXTRACTOR="python3 ~/my-extractor.py"
```

The extractor receives `ROLE: text` lines on stdin and must output:
```json
[{"content": "...", "type": "semantic", "importance": 0.8}]
```

## Core Agent Workflow

### 1. Start of Conversation — Recall Relevant Context

Retrieve up to `--budget` tokens of context before answering:

```bash
# natural language query, budget in tokens
gimem recall "user preferences and recent project decisions" --budget 4000

# with JSON output for tool-use parsing
gimem recall "deployment pipeline setup" --budget 2000 --json
```

Call `recall` at conversation start and whenever the topic shifts significantly.

### 2. Store Something Worth Remembering

```bash
gimem remember "User confirmed they use pnpm, not npm" --type episodic
gimem remember "User prefers functional React components with explicit return types" --type semantic
gimem remember "Deploy: run pnpm build, then rsync dist/ to /var/www/app, then systemctl restart nginx" --type procedural
```

Store memories immediately after learning something important -- do not batch at end of session. See [When to Remember](#when-to-remember) for specific trigger signals.

### 3. Track the Current Task — Working Memory

Replace working memory with the current task context:

```bash
gimem set-working "Implementing OAuth2 PKCE flow for the mobile app — currently on step 3: token exchange endpoint"
```

Clear (archive) working memory when the task is done:

```bash
gimem clear-working
```

### 4. End of Conversation -- Consolidate

Close the session to consolidate episodic memories into semantic ones:

```bash
SESSION=$(gimem start-session "Refactoring auth module to support SSO" --json | jq -r '.session_id')

# ... do work, store episodic memories with gimem remember ...

gimem end-session "$SESSION"
```

Alternatively, configure the Stop hook (see [Automatic Extraction (Stop Hook)](#automatic-extraction-stop-hook)) to let `gimem extract-from-transcript` handle end-of-session extraction automatically when Claude Code exits.

Run `consolidate` manually to batch-promote old episodic memories without a full session close:

```bash
gimem consolidate
```

## Quick Reference

| Command | Description |
|---|---|
| `gimem bootstrap` | Create required GitHub labels (run once per repo) |
| `gimem remember "<content>" --type <type>` | Store a memory (`episodic`, `semantic`, `procedural`, `working`) |
| `gimem recall "<query>" --budget <tokens>` | Retrieve relevant memories within token budget |
| `gimem forget <issue-number>` | Hard-delete a memory (closes + deletes the issue) |
| `gimem set-working "<content>"` | Replace current working memory |
| `gimem clear-working` | Archive the current working memory |
| `gimem start-session "<desc>"` | Open a session issue; prints `session-id` |
| `gimem end-session <session-id>` | Consolidate episodic memories and close session |
| `gimem consolidate` | Batch-promote episodic → semantic memories |
| `gimem evict [--dry-run]` | Archive low-retention memories to reduce noise |
| `gimem extract-from-transcript <path>` | Extract and store memories from a Claude Code session transcript |

### Global Flags

| Flag | Env Var | Description |
|---|---|---|
| `--token <tok>` | `GITHUB_TOKEN` | GitHub personal access token with `repo` scope |
| `--repo <owner/name>` | `GIMEM_REPO` | Target repo in `owner/name` format |
| `--user <username>` | `GIMEM_USER` | GitHub username for filtering own memories |
| `--json` | — | Emit structured JSON instead of human-readable text |

## JSON Output

Add `--json` to any command for machine-readable output suitable for tool-use parsing.

### `recall --json`

```json
{
  "memories": [
    {
      "issue_number": 42,
      "type": "semantic",
      "content": "User prefers functional React components with explicit return types",
      "created_at": "2025-04-10T14:23:00Z",
      "score": 0.91
    },
    {
      "issue_number": 37,
      "type": "procedural",
      "content": "Deploy: run pnpm build, rsync dist/ to /var/www/app, systemctl restart nginx",
      "created_at": "2025-03-28T09:11:00Z",
      "score": 0.74
    }
  ],
  "total_tokens": 312,
  "budget": 4000
}
```

### `remember --json`

```json
{
  "issue_number": 55,
  "type": "episodic",
  "content": "User confirmed they use pnpm, not npm",
  "url": "https://github.com/myorg/agent-memory/issues/55"
}
```

### `start-session --json`

```json
{
  "session_id": "78",
  "description": "Refactoring auth module to support SSO",
  "url": "https://github.com/myorg/agent-memory/issues/78"
}
```

## Tips for Claude and Codex

### When to Recall

- **Always** at the start of a new conversation before generating any response
- When the user mentions a topic you may have encountered before (project names, tech stack, preferences)
- Before making architectural or tooling recommendations — check stored preferences first
- After a long conversation where context may have scrolled out of the window

### When to Remember

Call `gimem remember` immediately when you observe any of these signals in the conversation:

- User corrects you ("no, we use X not Y", "actually it's", "that's wrong")
- User states a preference ("I prefer", "I always", "I like", "I want", "I love", "I hate")
- User issues a standing constraint ("never", "don't", "always avoid", "we don't do that")
- User reveals stack or tooling ("we use X", "our repo is", "our DB is", "we deploy to")
- A non-obvious convention is established (naming, structure, file layout, process)
- A procedure is agreed upon that should be repeatable (deploy steps, test commands, review checklists)
- User confirms a decision ("yes, go with X", "let's use X", "stick with X")

### Chaining Commands

```bash
# recall first, before starting work
gimem recall "UI framework and theming approach" --budget 3000

SESSION=$(gimem start-session "Add dark mode support" --json | jq -r '.session_id')

gimem remember "User prefers system-level dark mode detection" --type episodic

gimem remember "Dark mode implemented via CSS custom properties; toggle stored in localStorage" --type semantic
gimem end-session "$SESSION"
```

### Memory Type Selection Guide

| Situation | Type |
|---|---|
| "User just told me X in this conversation" | `episodic` |
| "This is a fact/preference that should always apply" | `semantic` |
| "This is a repeatable process or recipe" | `procedural` |
| "This is what I'm doing right now" | `working` |

### Semantic Search Rate Limit

GitHub's semantic search endpoint is rate-limited to **10 requests/minute**. Space `recall` calls at least 6 seconds apart if calling in a tight loop. All other operations (`remember`, `forget`, `set-working`, etc.) use standard GitHub API endpoints with a 5,000 requests/hour limit and do not need throttling.
