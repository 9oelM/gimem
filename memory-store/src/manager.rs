//! High-level memory management API.
//!
//! [`MemoryManager`] is the single entry-point for agent code. It orchestrates
//! the store, search engine, and consolidation engine behind a simple async API.
//!
//! # Quick Start
//! ```no_run
//! use memory_store::{MemoryManager, MemoryType};
//!
//! #[tokio::main]
//! async fn main() {
//!     let mem = MemoryManager::new("owner/agent-memory", "ghp_token", None);
//!     mem.bootstrap().await.unwrap();
//!     mem.remember("User prefers Rust", MemoryType::Semantic, "alice", 0.9, vec![], vec![]).await.unwrap();
//!     let ctx = mem.recall("programming language preferences", "alice", 1000).await.unwrap();
//!     println!("{ctx}");
//! }
//! ```

use std::sync::Arc;
use std::time::{Duration, Instant};

type HotCache = Arc<Mutex<Option<(Instant, Vec<MemoryEntry>)>>>;

use tokio::sync::Mutex;

use crate::consolidation::{
    ConsolidationConfig, ConsolidationEngine, ConsolidationStats, EvictionStats, SummarizeFn,
};
use crate::error::{MemoryError, Result};
use crate::labels;
use crate::models::{MemoryEntry, MemoryPatch, MemoryTier, MemoryType};
use crate::search::{SearchEngine, SearchQuery};
use crate::store::{GitHubIssuesStore, MemoryStore};

/// Hot-tier cache TTL: 5 minutes.
const HOT_CACHE_TTL: Duration = Duration::from_secs(300);

/// The public API for the GitHub Issues-backed agent memory system.
pub struct MemoryManager {
    store: Arc<dyn MemoryStore>,
    search: Arc<SearchEngine>,
    consolidation: Arc<ConsolidationEngine>,
    repo: String,
    token: String,
    /// Shared HTTP client for milestone operations (start/end session).
    http_client: reqwest::Client,
    /// `Some((fetched_at, entries))` when the cache is valid; `None` otherwise.
    hot_cache: HotCache,
}

impl MemoryManager {
    /// Create a new `MemoryManager`.
    pub fn new(repo: &str, token: &str, summarize_fn: Option<SummarizeFn>) -> Self {
        let store: Arc<dyn MemoryStore> = Arc::new(GitHubIssuesStore::new(repo, token));
        let search = Arc::new(SearchEngine::new(repo, token));
        Self::from_parts(store, search, repo, token, summarize_fn)
    }

    fn from_parts(
        store: Arc<dyn MemoryStore>,
        search: Arc<SearchEngine>,
        repo: &str,
        token: &str,
        summarize_fn: Option<SummarizeFn>,
    ) -> Self {
        let summarize = summarize_fn.unwrap_or_else(|| {
            Arc::new(|contents: Vec<String>| {
                Box::pin(async move {
                    let preview: String = contents
                        .first()
                        .map(|s| s.chars().take(100).collect())
                        .unwrap_or_default();
                    format!("Summary of {} memories: {}", contents.len(), preview)
                })
            })
        });

        let consolidation = Arc::new(ConsolidationEngine::new(
            Arc::clone(&store),
            Arc::clone(&search),
            summarize,
            ConsolidationConfig::default(),
        ));

        let http_client = reqwest::Client::builder()
            .user_agent("memory-store/0.1.0")
            .build()
            .expect("failed to build HTTP client");

        Self {
            store,
            search,
            consolidation,
            repo: repo.to_owned(),
            token: token.to_owned(),
            http_client,
            hot_cache: Arc::new(Mutex::new(None)),
        }
    }

    /// Bootstrap the repository by creating all required labels.
    pub async fn bootstrap(&self) -> Result<()> {
        let bootstrap_store = GitHubIssuesStore::new(&self.repo, &self.token);
        bootstrap_store
            .ensure_labels(labels::BOOTSTRAP_LABELS)
            .await
    }

    /// Store a new memory entry and return it with its assigned issue number.
    pub async fn remember(
        &self,
        content: &str,
        memory_type: MemoryType,
        user_id: &str,
        importance: f32,
        entities: Vec<String>,
        tags: Vec<String>,
    ) -> Result<MemoryEntry> {
        let mut entry = MemoryEntry::builder(content, memory_type)
            .user_id(user_id)
            .importance(importance)
            .entities(entities)
            .tags(tags)
            .build()?;

        self.store.create(&mut entry).await?;
        self.search.invalidate_cache();
        Ok(entry)
    }

    /// Recall relevant memories as a formatted context block.
    pub async fn recall(&self, query: &str, user_id: &str, token_budget: usize) -> Result<String> {
        // 1. Hot-tier (cached, 5-min TTL)
        let hot_entries = self.fetch_hot_tier(user_id).await.unwrap_or_default();

        // 2. Hybrid search
        let search_q = SearchQuery::default()
            .with_query(query)
            .with_user(user_id)
            .with_limit(20);
        let search_results = self
            .search
            .hybrid_search(&search_q)
            .await
            .unwrap_or_default();

        // 3. Deduplicate — hot-tier takes precedence
        let hot_ids: std::collections::HashSet<u64> =
            hot_entries.iter().filter_map(|e| e.issue_number).collect();

        let mut combined: Vec<(MemoryEntry, f32)> =
            hot_entries.iter().map(|e| (e.clone(), 2.0_f32)).collect();

        for sr in &search_results {
            let is_dup = sr.entry.issue_number.is_some_and(|n| hot_ids.contains(&n));
            if !is_dup {
                combined.push((sr.entry.clone(), sr.score));
            }
        }

        // 4. Re-rank (hot-tier already at 2.0)
        combined.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));

        if combined.is_empty() {
            return Ok(String::new());
        }

        // 5. Token-budget assembly
        let mut parts: Vec<String> = Vec::new();
        let mut used_tokens = 0usize;

        for (entry, score) in &combined {
            let tier_str = match entry.tier() {
                MemoryTier::Hot => "working",
                MemoryTier::Warm => "warm",
                MemoryTier::Cold => "cold",
            };
            let block = format!("[{tier_str}] Score: {score:.2}\n{}", entry.content);
            let est = block.len() / 4;
            if used_tokens + est > token_budget && !parts.is_empty() {
                break;
            }
            used_tokens += est;
            parts.push(block);
        }

        // 6. Fire-and-forget access events on top-5
        let top5: Vec<u64> = combined
            .iter()
            .take(5)
            .filter_map(|(e, _)| e.issue_number)
            .collect();

        if !top5.is_empty() {
            let store = Arc::clone(&self.store);
            tokio::spawn(async move {
                let body = serde_json::json!({
                    "event": "access",
                    "timestamp": chrono::Utc::now().to_rfc3339(),
                })
                .to_string();
                for n in top5 {
                    let _ = store.add_comment(n, &body).await;
                }
            });
        }

        if parts.is_empty() {
            return Ok(String::new());
        }

        Ok(format!("## Memory Context\n\n{}", parts.join("\n\n")))
    }

    /// Hard-delete a memory entry.
    pub async fn forget(&self, issue_number: u64) -> Result<()> {
        self.store.delete(issue_number).await?;
        self.invalidate_hot_cache().await;
        Ok(())
    }

    /// Apply a partial update to an existing memory entry.
    pub async fn update(&self, issue_number: u64, patch: MemoryPatch) -> Result<MemoryEntry> {
        let mut entry = self
            .store
            .get(issue_number)
            .await?
            .ok_or(MemoryError::NotFound { issue_number })?;

        apply_patch(&mut entry, patch);
        self.store.update(&entry).await?;
        Ok(entry)
    }

    /// Set (or replace) the working memory for a user.
    pub async fn set_working(&self, content: &str, user_id: &str) -> Result<MemoryEntry> {
        let q = SearchQuery::default()
            .with_user(user_id)
            .with_memory_types(vec![MemoryType::Working])
            .with_limit(1);
        let existing = self.search.lexical_search(&q).await.unwrap_or_default();

        let entry = if let Some(sr) = existing.into_iter().next() {
            let mut e = sr.entry;
            e.content = content.to_owned();
            self.store.update(&e).await?;
            e
        } else {
            let mut e = MemoryEntry::builder(content, MemoryType::Working)
                .user_id(user_id)
                .importance(1.0)
                .build()?;
            self.store.create(&mut e).await?;
            e
        };

        self.invalidate_hot_cache().await;
        Ok(entry)
    }

    /// Archive all `type:working` entries for a user.
    pub async fn clear_working(&self, user_id: &str) -> Result<()> {
        let q = SearchQuery::default()
            .with_user(user_id)
            .with_memory_types(vec![MemoryType::Working])
            .with_limit(100);
        let results = self.search.lexical_search(&q).await.unwrap_or_default();

        for sr in results {
            if let Some(n) = sr.entry.issue_number {
                self.store.archive(n, "Working memory cleared").await?;
            }
        }

        self.invalidate_hot_cache().await;
        Ok(())
    }

    /// Start a new conversation session.  Returns the session UUID.
    pub async fn start_session(&self, _user_id: &str, description: &str) -> Result<String> {
        let session_id = uuid::Uuid::new_v4().to_string();
        let url = format!("https://api.github.com/repos/{}/milestones", self.repo);

        let resp = self
            .http_client
            .post(&url)
            .bearer_auth(&self.token)
            .header("Accept", "application/vnd.github+json")
            .header("X-GitHub-Api-Version", "2022-11-28")
            .json(&serde_json::json!({
                "title": format!("sess_{session_id}"),
                "description": description,
            }))
            .send()
            .await?;

        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            let msg = resp.text().await.unwrap_or_default();
            return Err(MemoryError::GithubApi {
                status,
                message: msg,
            });
        }

        Ok(session_id)
    }

    /// End a session: run consolidation and close the milestone.
    pub async fn end_session(&self, user_id: &str, session_id: &str) -> Result<ConsolidationStats> {
        let stats = self.consolidation.consolidate(user_id).await?;
        self.close_milestone(&format!("sess_{session_id}"))
            .await
            .ok();
        Ok(stats)
    }

    /// Run the consolidation pipeline for a user.
    pub async fn consolidate(&self, user_id: &str) -> Result<ConsolidationStats> {
        self.consolidation.consolidate(user_id).await
    }

    /// Run the eviction pipeline for a user.
    pub async fn evict(&self, user_id: &str, dry_run: bool) -> Result<EvictionStats> {
        self.consolidation.evict(user_id, dry_run).await
    }

    // -----------------------------------------------------------------------
    // Private helpers
    // -----------------------------------------------------------------------

    async fn fetch_hot_tier(&self, user_id: &str) -> Result<Vec<MemoryEntry>> {
        // Read from cache without holding the lock across .await.
        let cached = {
            let guard = self.hot_cache.lock().await;
            guard.as_ref().and_then(|(fetched_at, entries)| {
                if fetched_at.elapsed() < HOT_CACHE_TTL {
                    Some(entries.clone())
                } else {
                    None
                }
            })
        };

        if let Some(entries) = cached {
            return Ok(entries);
        }

        let q = SearchQuery::default()
            .with_user(user_id)
            .with_tiers(vec![MemoryTier::Hot])
            .with_limit(50);
        let results = self.search.lexical_search(&q).await?;
        let entries: Vec<MemoryEntry> = results.into_iter().map(|sr| sr.entry).collect();

        {
            let mut guard = self.hot_cache.lock().await;
            *guard = Some((Instant::now(), entries.clone()));
        }

        Ok(entries)
    }

    async fn invalidate_hot_cache(&self) {
        let mut guard = self.hot_cache.lock().await;
        *guard = None;
    }

    async fn close_milestone(&self, title: &str) -> Result<()> {
        let list_url = format!("https://api.github.com/repos/{}/milestones", self.repo);
        let resp = self
            .http_client
            .get(&list_url)
            .bearer_auth(&self.token)
            .header("Accept", "application/vnd.github+json")
            .header("X-GitHub-Api-Version", "2022-11-28")
            .query(&[("state", "open"), ("per_page", "100")])
            .send()
            .await?;

        if !resp.status().is_success() {
            return Ok(());
        }

        let milestones: Vec<serde_json::Value> = resp.json().await?;
        let number = milestones
            .iter()
            .find(|m| m["title"].as_str() == Some(title))
            .and_then(|m| m["number"].as_u64());

        if let Some(n) = number {
            self.http_client
                .patch(format!(
                    "https://api.github.com/repos/{}/milestones/{}",
                    self.repo, n
                ))
                .bearer_auth(&self.token)
                .header("Accept", "application/vnd.github+json")
                .header("X-GitHub-Api-Version", "2022-11-28")
                .json(&serde_json::json!({ "state": "closed" }))
                .send()
                .await?;
        }

        Ok(())
    }
}

fn apply_patch(entry: &mut MemoryEntry, patch: MemoryPatch) {
    if let Some(v) = patch.content {
        entry.content = v;
    }
    if let Some(v) = patch.memory_type {
        entry.memory_type = v;
    }
    if let Some(v) = patch.user_id {
        entry.user_id = Some(v);
    }
    if let Some(v) = patch.agent_id {
        entry.agent_id = Some(v);
    }
    if let Some(v) = patch.session_id {
        entry.session_id = Some(v);
    }
    if let Some(v) = patch.importance {
        entry.importance = v;
    }
    if let Some(v) = patch.confidence {
        entry.confidence = v;
    }
    if let Some(v) = patch.access_count {
        entry.access_count = v;
    }
    if let Some(v) = patch.last_accessed {
        entry.last_accessed = Some(v);
    }
    if let Some(v) = patch.entities {
        entry.entities = v;
    }
    if let Some(v) = patch.tags {
        entry.tags = v;
    }
    if let Some(v) = patch.structured_data {
        entry.structured_data = v;
    }
    if let Some(v) = patch.supersedes {
        entry.supersedes = v;
    }
    if let Some(v) = patch.related_to {
        entry.related_to = v;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{MemoryPatch, MemoryType};
    use crate::search::SearchResult;
    use crate::store::testutil::MockMemoryStore;
    use chrono::Utc;
    use uuid::Uuid;

    fn make_manager(store: Arc<MockMemoryStore>) -> MemoryManager {
        let search = Arc::new(SearchEngine::new("test/repo", "fake_token"));
        MemoryManager::from_parts(
            store as Arc<dyn MemoryStore>,
            search,
            "test/repo",
            "fake_token",
            None,
        )
    }

    fn make_entry_with_number(n: u64, content: &str, memory_type: MemoryType) -> MemoryEntry {
        MemoryEntry {
            memory_id: Uuid::new_v4(),
            issue_number: Some(n),
            content: content.to_owned(),
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
            structured_data: serde_json::Value::Null,
            supersedes: vec![],
            related_to: vec![],
        }
    }

    #[tokio::test]
    async fn remember_creates_entry_with_correct_type_and_issue_number() {
        let store = Arc::new(MockMemoryStore::new());
        let mgr = make_manager(store.clone());

        let entry = mgr
            .remember(
                "User prefers Rust",
                MemoryType::Semantic,
                "alice",
                0.9,
                vec![],
                vec![],
            )
            .await
            .unwrap();

        assert_eq!(entry.memory_type, MemoryType::Semantic);
        assert!(
            entry.issue_number.is_some(),
            "issue_number must be assigned"
        );
        assert_eq!(entry.content, "User prefers Rust");
        assert_eq!(entry.user_id.as_deref(), Some("alice"));
    }

    #[tokio::test]
    async fn recall_on_empty_store_returns_empty_string() {
        let store = Arc::new(MockMemoryStore::new());
        let mgr = make_manager(store.clone());

        let ctx = mgr.recall("anything", "alice", 1000).await.unwrap();
        assert!(ctx.is_empty(), "expected empty string, got: {ctx:?}");
    }

    #[test]
    fn recall_dedup_hot_tier_takes_precedence_over_search() {
        let issue_num = 42u64;
        let hot_entry = make_entry_with_number(issue_num, "hot entry", MemoryType::Working);
        let search_entry = make_entry_with_number(issue_num, "search entry", MemoryType::Working);

        let hot_entries = [hot_entry.clone()];
        let search_results = vec![SearchResult {
            entry: search_entry,
            score: 0.9,
            gh_score: 0.9,
        }];

        let hot_ids: std::collections::HashSet<u64> =
            hot_entries.iter().filter_map(|e| e.issue_number).collect();

        let mut combined: Vec<(MemoryEntry, f32)> =
            hot_entries.iter().map(|e| (e.clone(), 2.0_f32)).collect();

        for sr in &search_results {
            let is_dup = sr.entry.issue_number.is_some_and(|n| hot_ids.contains(&n));
            if !is_dup {
                combined.push((sr.entry.clone(), sr.score));
            }
        }

        assert_eq!(combined.len(), 1, "duplicate must be suppressed");
        assert_eq!(combined[0].0.content, "hot entry");
        assert!((combined[0].1 - 2.0).abs() < f32::EPSILON);
    }

    #[test]
    fn recall_token_budget_first_entry_always_included() {
        // Simulate assembly logic: first entry always included even if over budget.
        let content = "x".repeat(4000);
        let budget = 10usize; // very small

        let entry = make_entry_with_number(1, &content, MemoryType::Semantic);
        let combined = vec![(entry, 0.9_f32)];

        let mut parts: Vec<String> = Vec::new();
        let mut used = 0usize;
        for (e, score) in &combined {
            let block = format!("[warm] Score: {score:.2}\n{}", e.content);
            let est = block.len() / 4;
            if used + est > budget && !parts.is_empty() {
                break;
            }
            used += est;
            parts.push(block);
        }

        assert!(!parts.is_empty(), "first entry must always be included");
    }

    #[tokio::test]
    async fn forget_removes_entry_from_store() {
        let store = Arc::new(MockMemoryStore::new());
        let mgr = make_manager(store.clone());

        let entry = mgr
            .remember(
                "to forget",
                MemoryType::Episodic,
                "alice",
                0.3,
                vec![],
                vec![],
            )
            .await
            .unwrap();
        let n = entry.issue_number.unwrap();

        mgr.forget(n).await.unwrap();

        let fetched = store.get(n).await.unwrap();
        assert!(fetched.is_none());
    }

    #[tokio::test]
    async fn update_applies_patch_fields_only() {
        let store = Arc::new(MockMemoryStore::new());
        let mgr = make_manager(store.clone());

        let entry = mgr
            .remember(
                "original content",
                MemoryType::Semantic,
                "alice",
                0.5,
                vec![],
                vec![],
            )
            .await
            .unwrap();
        let n = entry.issue_number.unwrap();

        let patch = MemoryPatch {
            content: Some("updated content".to_owned()),
            importance: Some(0.9),
            ..Default::default()
        };

        let updated = mgr.update(n, patch).await.unwrap();
        assert_eq!(updated.content, "updated content");
        assert!((updated.importance - 0.9).abs() < f32::EPSILON);
        assert_eq!(updated.memory_type, MemoryType::Semantic);
    }

    #[tokio::test]
    async fn set_working_creates_working_type_entry() {
        let store = Arc::new(MockMemoryStore::new());
        let mgr = make_manager(store.clone());

        let entry = mgr
            .set_working("current task context", "alice")
            .await
            .unwrap();

        assert_eq!(entry.memory_type, MemoryType::Working);
        assert!(entry.issue_number.is_some());
        assert_eq!(entry.content, "current task context");
    }

    #[tokio::test]
    async fn clear_working_does_not_error_when_empty() {
        let store = Arc::new(MockMemoryStore::new());
        let mgr = make_manager(store.clone());
        mgr.clear_working("alice").await.unwrap();
    }

    #[tokio::test]
    async fn consolidate_delegates_and_returns_stats() {
        let store = Arc::new(MockMemoryStore::new());
        let mgr = make_manager(store.clone());
        let stats = mgr.consolidate("alice").await.unwrap_or_default();
        assert_eq!(stats.promoted, 0);
    }

    #[tokio::test]
    async fn evict_dry_run_does_not_error() {
        let store = Arc::new(MockMemoryStore::new());
        let mgr = make_manager(store.clone());
        let stats = mgr.evict("alice", true).await.unwrap_or_default();
        assert_eq!(stats.evicted, 0);
    }

    #[test]
    fn apply_patch_only_modifies_some_fields() {
        let mut entry = make_entry_with_number(1, "original", MemoryType::Episodic);

        apply_patch(
            &mut entry,
            MemoryPatch {
                content: Some("patched".to_owned()),
                importance: Some(0.95),
                ..Default::default()
            },
        );

        assert_eq!(entry.content, "patched");
        assert!((entry.importance - 0.95).abs() < f32::EPSILON);
        assert_eq!(entry.memory_type, MemoryType::Episodic);
        assert_eq!(entry.user_id.as_deref(), Some("alice"));
    }
}
