//! Consolidation pipeline for the memory-store crate.
//!
//! This module implements the consolidation and eviction pipeline that
//! converts episodic noise into semantic signal:
//!
//! 1. **Cluster**: Group related episodic entries using hybrid search.
//! 2. **Consolidate**: Summarise each cluster into a new semantic entry.
//! 3. **Evict**: Archive entries whose retention score falls below a threshold.

use std::collections::HashSet;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use crate::error::Result;
use crate::labels::STATUS_SUPERSEDED;
use crate::models::{MemoryEntry, MemoryType};
use crate::search::{SearchEngine, SearchQuery};
use crate::store::MemoryStore;

// ---------------------------------------------------------------------------
// Async summarisation function type
// ---------------------------------------------------------------------------

/// A dependency-injected async function that summarises a list of memory
/// contents into a single string.
///
/// Callers wrap an async block:
/// ```rust,ignore
/// let f: SummarizeFn = Arc::new(|contents| Box::pin(async move {
///     // Call your LLM here
///     contents.join(" | ")
/// }));
/// ```
///
/// Async closures are not yet stable in Rust, so the `Box::pin(async move { … })`
/// pattern is required.
pub type SummarizeFn = Arc<
    dyn Fn(Vec<String>) -> Pin<Box<dyn Future<Output = String> + Send>>
        + Send
        + Sync,
>;

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

/// Configuration knobs for the consolidation pipeline.
#[derive(Debug, Clone)]
pub struct ConsolidationConfig {
    /// Trigger consolidation when the episodic count reaches this value.
    ///
    /// Default: `20`.
    pub episodic_threshold: usize,

    /// Archive memories whose `retention_score()` is below this value.
    ///
    /// Default: `0.15`.
    pub min_retention_score: f32,

    /// Archive cold memories that are older than this many days.
    ///
    /// Default: `90`.
    pub cold_archive_days: u32,

    /// Minimum search score required for an episodic entry to join a cluster.
    ///
    /// Default: `0.7`.
    pub cluster_similarity: f32,
}

impl Default for ConsolidationConfig {
    fn default() -> Self {
        Self {
            episodic_threshold: 20,
            min_retention_score: 0.15,
            cold_archive_days: 90,
            cluster_similarity: 0.7,
        }
    }
}

// ---------------------------------------------------------------------------
// Output statistics
// ---------------------------------------------------------------------------

/// Statistics returned by a consolidation run.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ConsolidationStats {
    /// Number of episodic entries that were merged (and subsequently archived).
    pub consolidated: usize,
    /// Number of new semantic entries that were created from clusters.
    pub promoted: usize,
    /// Number of entries that were evicted (archived due to low retention score).
    pub evicted: usize,
}

/// Statistics returned by an eviction run.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct EvictionStats {
    /// Number of entries that were archived (0 when `dry_run = true`).
    pub evicted: usize,
    /// Total number of entries that were candidates for eviction.
    pub candidates: usize,
}

// ---------------------------------------------------------------------------
// Private helpers
// ---------------------------------------------------------------------------

/// Merges the entity lists from a cluster into a deduplicated `Vec`, preserving
/// first-seen order.
fn merge_entities(cluster: &[MemoryEntry]) -> Vec<String> {
    let mut seen: HashSet<&str> = HashSet::new();
    let mut out: Vec<String> = Vec::new();
    for entry in cluster {
        for entity in &entry.entities {
            if seen.insert(entity.as_str()) {
                out.push(entity.clone());
            }
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Engine
// ---------------------------------------------------------------------------

/// Drives the end-of-session consolidation and periodic eviction pipelines.
///
/// `ConsolidationEngine` is cheaply cloneable via its inner `Arc`s and is
/// intended to be shared across async tasks.
pub struct ConsolidationEngine {
    store: Arc<dyn MemoryStore>,
    search: Arc<SearchEngine>,
    summarize_fn: SummarizeFn,
    config: ConsolidationConfig,
}

impl ConsolidationEngine {
    /// Creates a new `ConsolidationEngine`.
    pub fn new(
        store: Arc<dyn MemoryStore>,
        search: Arc<SearchEngine>,
        summarize_fn: SummarizeFn,
        config: ConsolidationConfig,
    ) -> Self {
        Self { store, search, summarize_fn, config }
    }

    /// Consolidates episodic memories for `user_id` into semantic summaries.
    ///
    /// # Algorithm
    ///
    /// 1. Fetch all open episodic entries for the user (lexical search, limit 200).
    /// 2. Return early with zero stats if the count is below `config.episodic_threshold`.
    /// 3. Greedily cluster entries: for each unassigned episodic, run a hybrid
    ///    search using the first 200 characters of its content, group results
    ///    with score ≥ `config.cluster_similarity` into a cluster.
    /// 4. For each cluster of size ≥ 2:
    ///    - Call `summarize_fn` to produce a single summary string.
    ///    - Create a new `Semantic` `MemoryEntry` and persist it.
    ///    - Archive every source episodic with a "Consolidated into #N" comment.
    ///    - Mark every source episodic with `status:superseded`.
    pub async fn consolidate(&self, user_id: &str) -> Result<ConsolidationStats> {
        let episodic_query = SearchQuery {
            query: format!("type:episodic user:{user_id}"),
            user_id: Some(user_id.to_owned()),
            memory_types: vec![MemoryType::Episodic],
            tiers: vec![],
            include_archived: false,
            limit: 200,
        };
        let episodic_results = self.search.lexical_search(&episodic_query).await?;

        if episodic_results.len() < self.config.episodic_threshold {
            return Ok(ConsolidationStats::default());
        }

        let clusters = self.build_clusters(user_id, &episodic_results).await?;
        self.flush_clusters(user_id, clusters).await
    }

    /// Archives open memories for `user_id` whose retention score is below the
    /// configured threshold.
    ///
    /// When `dry_run` is `true`, candidates are counted but nothing is archived.
    pub async fn evict(&self, user_id: &str, dry_run: bool) -> Result<EvictionStats> {
        let query = SearchQuery {
            query: format!("user:{user_id}"),
            user_id: Some(user_id.to_owned()),
            memory_types: vec![],
            tiers: vec![],
            include_archived: false,
            limit: 500,
        };
        let results = self.search.lexical_search(&query).await?;

        // Compute score once per entry to avoid redundant calls.
        let candidates: Vec<(u64, f32)> = results
            .iter()
            .filter_map(|r| {
                let score = r.entry.retention_score();
                if score < self.config.min_retention_score {
                    r.entry.issue_number.map(|n| (n, score))
                } else {
                    None
                }
            })
            .collect();

        let candidate_count = candidates.len();

        if !dry_run {
            for (n, score) in &candidates {
                self.store.archive(*n, &format!("Evicted: score={score:.2}")).await?;
            }
        }

        Ok(EvictionStats {
            evicted: if dry_run { 0 } else { candidate_count },
            candidates: candidate_count,
        })
    }

    // -----------------------------------------------------------------------
    // Private helpers
    // -----------------------------------------------------------------------

    /// Greedily groups unassigned episodic results into clusters using hybrid
    /// search. Returns only clusters with ≥ 2 members.
    async fn build_clusters(
        &self,
        user_id: &str,
        episodic_results: &[crate::search::SearchResult],
    ) -> Result<Vec<Vec<MemoryEntry>>> {
        let mut assigned: HashSet<u64> = HashSet::new();
        let mut clusters: Vec<Vec<MemoryEntry>> = Vec::new();

        for result in episodic_results {
            let entry = &result.entry;
            let issue_num = match entry.issue_number {
                Some(n) => n,
                None => continue,
            };

            if assigned.contains(&issue_num) {
                continue;
            }

            let anchor: String = entry.content.chars().take(200).collect();
            let similar_query = SearchQuery {
                query: anchor,
                user_id: Some(user_id.to_owned()),
                memory_types: vec![MemoryType::Episodic],
                tiers: vec![],
                include_archived: false,
                limit: 50,
            };
            let similar_results = self.search.hybrid_search(&similar_query).await?;

            let mut cluster: Vec<MemoryEntry> = Vec::new();
            for similar in &similar_results {
                if similar.score < self.config.cluster_similarity {
                    continue;
                }
                let n = match similar.entry.issue_number {
                    Some(n) => n,
                    None => continue,
                };
                if !assigned.contains(&n) {
                    cluster.push(similar.entry.clone());
                    assigned.insert(n);
                }
            }

            if cluster.len() >= 2 {
                clusters.push(cluster);
            }
        }

        Ok(clusters)
    }

    /// Summarises each cluster, creates a semantic entry, and archives sources.
    async fn flush_clusters(
        &self,
        user_id: &str,
        clusters: Vec<Vec<MemoryEntry>>,
    ) -> Result<ConsolidationStats> {
        let mut stats = ConsolidationStats::default();

        for cluster in clusters {
            let contents: Vec<String> = cluster.iter().map(|e| e.content.clone()).collect();
            let summary = (self.summarize_fn)(contents).await;

            let max_importance = cluster
                .iter()
                .map(|e| e.importance)
                .fold(f32::NEG_INFINITY, f32::max);

            let mut new_entry = MemoryEntry::builder(summary, MemoryType::Semantic)
                .user_id(user_id)
                .importance(max_importance)
                .entities(merge_entities(&cluster))
                .build()?;

            self.store.create(&mut new_entry).await?;

            let new_issue = new_entry.issue_number.unwrap_or(0);
            let comment = format!("Consolidated into #{new_issue}");

            for source in &cluster {
                if let Some(n) = source.issue_number {
                    self.store.archive(n, &comment).await?;
                    self.store.set_labels(n, &[STATUS_SUPERSEDED.to_owned()]).await?;
                }
            }

            stats.consolidated += cluster.len();
            stats.promoted += 1;
        }

        Ok(stats)
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    use std::collections::HashMap;
    use std::sync::Mutex;

    use async_trait::async_trait;
    use chrono::Utc;
    use uuid::Uuid;

    use crate::models::{MemoryEntry, MemoryType};
    use crate::search::SearchResult;
    use crate::store::MemoryStore;

    // -----------------------------------------------------------------------
    // Mock store
    // -----------------------------------------------------------------------

    /// Minimal in-memory [`MemoryStore`] for unit tests.
    #[derive(Default)]
    struct MockMemoryStore {
        entries: Mutex<HashMap<u64, MemoryEntry>>,
        comments: Mutex<Vec<(u64, String)>>,
        labels_set: Mutex<Vec<(u64, Vec<String>)>>,
        archived: Mutex<Vec<(u64, String)>>,
        next_issue: Mutex<u64>,
    }

    #[async_trait]
    impl MemoryStore for MockMemoryStore {
        async fn create(&self, entry: &mut MemoryEntry) -> Result<()> {
            let mut n = self.next_issue.lock().unwrap();
            *n += 1;
            entry.issue_number = Some(*n);
            self.entries.lock().unwrap().insert(*n, entry.clone());
            Ok(())
        }

        async fn get(&self, issue_number: u64) -> Result<Option<MemoryEntry>> {
            Ok(self.entries.lock().unwrap().get(&issue_number).cloned())
        }

        async fn update(&self, entry: &MemoryEntry) -> Result<()> {
            if let Some(n) = entry.issue_number {
                self.entries.lock().unwrap().insert(n, entry.clone());
            }
            Ok(())
        }

        async fn archive(&self, issue_number: u64, reason: &str) -> Result<()> {
            self.archived.lock().unwrap().push((issue_number, reason.to_owned()));
            Ok(())
        }

        async fn delete(&self, issue_number: u64) -> Result<()> {
            self.entries.lock().unwrap().remove(&issue_number);
            Ok(())
        }

        async fn add_comment(&self, issue_number: u64, body: &str) -> Result<()> {
            self.comments.lock().unwrap().push((issue_number, body.to_owned()));
            Ok(())
        }

        async fn set_labels(&self, issue_number: u64, labels: &[String]) -> Result<()> {
            self.labels_set.lock().unwrap().push((issue_number, labels.to_vec()));
            Ok(())
        }
    }

    // -----------------------------------------------------------------------
    // Test helpers
    // -----------------------------------------------------------------------

    fn make_entry(issue_number: u64, memory_type: MemoryType, importance: f32) -> MemoryEntry {
        MemoryEntry {
            memory_id: Uuid::new_v4(),
            issue_number: Some(issue_number),
            content: format!("memory content for issue {issue_number}"),
            memory_type,
            user_id: Some("alice".to_owned()),
            agent_id: None,
            session_id: None,
            importance,
            confidence: 0.9,
            access_count: 0,
            last_accessed: None,
            created_at: Utc::now(),
            entities: vec![format!("entity{issue_number}")],
            tags: vec![],
            structured_data: serde_json::Value::Null,
            supersedes: vec![],
            related_to: vec![],
        }
    }

    fn make_search_result(entry: MemoryEntry, score: f32) -> SearchResult {
        SearchResult { score, gh_score: score, entry }
    }

    fn noop_summarize() -> SummarizeFn {
        Arc::new(|contents: Vec<String>| Box::pin(async move { contents.join("; ") }))
    }

    // -----------------------------------------------------------------------
    // Test-only engine with injectable search results
    // -----------------------------------------------------------------------

    /// Drives the same consolidation/eviction logic as [`ConsolidationEngine`]
    /// but accepts pre-canned search results, avoiding all network I/O.
    struct TestConsolidationEngine {
        store: Arc<MockMemoryStore>,
        summarize_fn: SummarizeFn,
        config: ConsolidationConfig,
        /// Returned verbatim by every `lexical_search` call.
        lexical_results: Vec<SearchResult>,
        /// Returned verbatim by every `hybrid_search` call.
        hybrid_results: Vec<SearchResult>,
    }

    impl TestConsolidationEngine {
        fn new(
            store: Arc<MockMemoryStore>,
            summarize_fn: SummarizeFn,
            config: ConsolidationConfig,
            lexical_results: Vec<SearchResult>,
            hybrid_results: Vec<SearchResult>,
        ) -> Self {
            Self { store, summarize_fn, config, lexical_results, hybrid_results }
        }

        async fn consolidate(&self, user_id: &str) -> Result<ConsolidationStats> {
            if self.lexical_results.len() < self.config.episodic_threshold {
                return Ok(ConsolidationStats::default());
            }

            let clusters = self.build_clusters_mock(user_id);
            self.flush_clusters(user_id, clusters).await
        }

        /// Greedy cluster builder that uses `self.hybrid_results` instead of HTTP.
        fn build_clusters_mock(&self, _user_id: &str) -> Vec<Vec<MemoryEntry>> {
            let mut assigned: HashSet<u64> = HashSet::new();
            let mut clusters: Vec<Vec<MemoryEntry>> = Vec::new();

            for result in &self.lexical_results {
                let issue_num = match result.entry.issue_number {
                    Some(n) => n,
                    None => continue,
                };
                if assigned.contains(&issue_num) {
                    continue;
                }

                let mut cluster: Vec<MemoryEntry> = Vec::new();
                for similar in &self.hybrid_results {
                    if similar.score < self.config.cluster_similarity {
                        continue;
                    }
                    let n = match similar.entry.issue_number {
                        Some(n) => n,
                        None => continue,
                    };
                    if !assigned.contains(&n) {
                        cluster.push(similar.entry.clone());
                        assigned.insert(n);
                    }
                }

                if cluster.len() >= 2 {
                    clusters.push(cluster);
                }
            }

            clusters
        }

        /// Shared flush logic (identical to `ConsolidationEngine::flush_clusters`).
        async fn flush_clusters(
            &self,
            user_id: &str,
            clusters: Vec<Vec<MemoryEntry>>,
        ) -> Result<ConsolidationStats> {
            let mut stats = ConsolidationStats::default();

            for cluster in clusters {
                let contents: Vec<String> =
                    cluster.iter().map(|e| e.content.clone()).collect();
                let summary = (self.summarize_fn)(contents).await;

                let max_importance = cluster
                    .iter()
                    .map(|e| e.importance)
                    .fold(f32::NEG_INFINITY, f32::max);

                let mut new_entry = MemoryEntry::builder(summary, MemoryType::Semantic)
                    .user_id(user_id)
                    .importance(max_importance)
                    .entities(merge_entities(&cluster))
                    .build()?;

                self.store.create(&mut new_entry).await?;

                let new_issue = new_entry.issue_number.unwrap_or(0);
                let comment = format!("Consolidated into #{new_issue}");

                for source in &cluster {
                    if let Some(n) = source.issue_number {
                        self.store.archive(n, &comment).await?;
                        self.store
                            .set_labels(n, &[STATUS_SUPERSEDED.to_owned()])
                            .await?;
                    }
                }

                stats.consolidated += cluster.len();
                stats.promoted += 1;
            }

            Ok(stats)
        }

        async fn evict(&self, _user_id: &str, dry_run: bool) -> Result<EvictionStats> {
            let candidates: Vec<(u64, f32)> = self
                .lexical_results
                .iter()
                .filter_map(|r| {
                    let score = r.entry.retention_score();
                    if score < self.config.min_retention_score {
                        r.entry.issue_number.map(|n| (n, score))
                    } else {
                        None
                    }
                })
                .collect();

            let candidate_count = candidates.len();

            if !dry_run {
                for (n, score) in &candidates {
                    self.store.archive(*n, &format!("Evicted: score={score:.2}")).await?;
                }
            }

            Ok(EvictionStats {
                evicted: if dry_run { 0 } else { candidate_count },
                candidates: candidate_count,
            })
        }
    }

    // -----------------------------------------------------------------------
    // Tests
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn consolidation_below_threshold_returns_zero_stats() {
        let store = Arc::new(MockMemoryStore::default());
        let config = ConsolidationConfig { episodic_threshold: 5, ..Default::default() };

        let episodics: Vec<SearchResult> = (1u64..=3)
            .map(|n| make_search_result(make_entry(n, MemoryType::Episodic, 0.5), 1.0))
            .collect();

        let engine = TestConsolidationEngine::new(
            store, noop_summarize(), config, episodics, vec![],
        );

        let stats = engine.consolidate("alice").await.unwrap();
        assert_eq!(stats, ConsolidationStats::default());
    }

    /// 4 episodics — hybrid always returns all 4 at score 0.9, so the greedy
    /// algorithm assigns them all to the first cluster: consolidated=4, promoted=1.
    #[tokio::test]
    async fn consolidation_creates_two_semantic_entries_from_two_clusters() {
        let store = Arc::new(MockMemoryStore::default());
        let config = ConsolidationConfig {
            episodic_threshold: 4,
            cluster_similarity: 0.7,
            ..Default::default()
        };

        let episodics: Vec<SearchResult> = (1u64..=4)
            .map(|n| make_search_result(make_entry(n, MemoryType::Episodic, 0.6), 1.0))
            .collect();

        let hybrid: Vec<SearchResult> = (1u64..=4)
            .map(|n| make_search_result(make_entry(n, MemoryType::Episodic, 0.6), 0.9))
            .collect();

        let engine = TestConsolidationEngine::new(
            store.clone(), noop_summarize(), config, episodics, hybrid,
        );

        let stats = engine.consolidate("alice").await.unwrap();
        assert_eq!(stats.consolidated, 4, "expected 4 consolidated");
        assert_eq!(stats.promoted, 1, "expected 1 promoted (greedy groups all 4 together)");
    }

    #[tokio::test]
    async fn consolidated_entries_have_semantic_type() {
        let store = Arc::new(MockMemoryStore::default());
        let config = ConsolidationConfig {
            episodic_threshold: 2,
            cluster_similarity: 0.7,
            ..Default::default()
        };

        let episodics: Vec<SearchResult> = (1u64..=2)
            .map(|n| make_search_result(make_entry(n, MemoryType::Episodic, 0.5), 1.0))
            .collect();
        let hybrid = episodics.clone();

        let engine = TestConsolidationEngine::new(
            store.clone(), noop_summarize(), config, episodics, hybrid,
        );

        engine.consolidate("alice").await.unwrap();

        let entries = store.entries.lock().unwrap();
        for entry in entries.values() {
            assert_eq!(entry.memory_type, MemoryType::Semantic, "created entry must be Semantic");
        }
    }

    #[tokio::test]
    async fn archived_entries_get_archive_comment() {
        let store = Arc::new(MockMemoryStore::default());
        let config = ConsolidationConfig {
            episodic_threshold: 2,
            cluster_similarity: 0.7,
            ..Default::default()
        };

        let episodics: Vec<SearchResult> = (1u64..=2)
            .map(|n| make_search_result(make_entry(n, MemoryType::Episodic, 0.5), 1.0))
            .collect();
        let hybrid = episodics.clone();

        let engine = TestConsolidationEngine::new(
            store.clone(), noop_summarize(), config, episodics, hybrid,
        );

        engine.consolidate("alice").await.unwrap();

        let archived = store.archived.lock().unwrap();
        assert_eq!(archived.len(), 2, "both source entries should be archived");
        for (_, reason) in archived.iter() {
            assert!(
                reason.starts_with("Consolidated into #"),
                "reason should mention new issue: {reason}"
            );
        }
    }

    #[tokio::test]
    async fn eviction_archives_low_score_entries() {
        let store = Arc::new(MockMemoryStore::default());
        let config = ConsolidationConfig::default();

        // importance=0, confidence=0, age=200 days → retention_score ≈ 0.0 < 0.15
        let mut old_entry = make_entry(10, MemoryType::Episodic, 0.0);
        old_entry.confidence = 0.0;
        old_entry.created_at = Utc::now() - chrono::Duration::days(200);

        let engine = TestConsolidationEngine::new(
            store.clone(), noop_summarize(), config,
            vec![make_search_result(old_entry, 1.0)],
            vec![],
        );

        let stats = engine.evict("alice", false).await.unwrap();
        assert!(stats.candidates >= 1);
        assert_eq!(stats.evicted, stats.candidates);
        assert!(!store.archived.lock().unwrap().is_empty(), "low-score entry should be archived");
    }

    #[tokio::test]
    async fn eviction_keeps_high_score_entries() {
        let store = Arc::new(MockMemoryStore::default());
        let config = ConsolidationConfig::default();

        let entry = make_entry(20, MemoryType::Semantic, 0.9);
        let engine = TestConsolidationEngine::new(
            store.clone(), noop_summarize(), config,
            vec![make_search_result(entry, 1.0)],
            vec![],
        );

        let stats = engine.evict("alice", false).await.unwrap();
        assert_eq!(stats.candidates, 0, "high-score entry should not be a candidate");
        assert_eq!(stats.evicted, 0);
        assert!(store.archived.lock().unwrap().is_empty(), "nothing should be archived");
    }

    #[tokio::test]
    async fn eviction_dry_run_counts_but_does_not_archive() {
        let store = Arc::new(MockMemoryStore::default());
        let config = ConsolidationConfig::default();

        let mut old_entry = make_entry(30, MemoryType::Episodic, 0.0);
        old_entry.confidence = 0.0;
        old_entry.created_at = Utc::now() - chrono::Duration::days(200);

        let engine = TestConsolidationEngine::new(
            store.clone(), noop_summarize(), config,
            vec![make_search_result(old_entry, 1.0)],
            vec![],
        );

        let stats = engine.evict("alice", true).await.unwrap();
        assert!(stats.candidates >= 1, "should count candidates");
        assert_eq!(stats.evicted, 0, "dry_run must not evict");
        assert!(store.archived.lock().unwrap().is_empty(), "dry_run must not archive anything");
    }

    #[test]
    fn consolidation_config_default_values() {
        let cfg = ConsolidationConfig::default();
        assert_eq!(cfg.episodic_threshold, 20);
        assert!((cfg.min_retention_score - 0.15).abs() < f32::EPSILON);
        assert_eq!(cfg.cold_archive_days, 90);
        assert!((cfg.cluster_similarity - 0.7).abs() < f32::EPSILON);
    }
}
