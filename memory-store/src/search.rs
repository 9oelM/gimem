//! Search engine for GitHub Issues-backed agent memory.
//!
//! Provides:
//! - [`TokenBucket`] — token bucket rate limiter for the 10 req/min semantic search quota.
//! - [`SearchQuery`] — query builder for memory search operations.
//! - [`SearchResult`] — ranked result pairing a [`crate::models::MemoryEntry`] with scores.
//! - [`SearchEngine`] — core search driver: hybrid (semantic+keyword) and lexical modes.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::Mutex;
use tokio::time::sleep;

use crate::error::{MemoryError, Result};
use crate::models::{MemoryEntry, MemoryTier, MemoryType};

// ---------------------------------------------------------------------------
// TokenBucket
// ---------------------------------------------------------------------------

/// A token bucket rate limiter.
///
/// Designed for GitHub's semantic/hybrid search quota (10 req/min). Refills at
/// `refill_rate` tokens per second, capped at `capacity`.
///
/// **Never hold the `Mutex<TokenBucket>` across an `.await`.**
/// Correct pattern: lock → `try_acquire` → drop lock → sleep if empty → repeat.
pub struct TokenBucket {
    capacity: f32,
    tokens: f32,
    /// Tokens added per second.
    refill_rate: f32,
    last_refill: Instant,
}

impl TokenBucket {
    /// Creates a new `TokenBucket` starting at full capacity.
    pub fn new(capacity: f32, refill_rate: f32) -> Self {
        Self {
            capacity,
            tokens: capacity,
            refill_rate,
            last_refill: Instant::now(),
        }
    }

    /// Attempts to consume one token.
    ///
    /// Refills based on elapsed time (capped at `capacity`), then returns `true`
    /// and decrements if a token is available, or `false` if the bucket is empty.
    pub fn try_acquire(&mut self) -> bool {
        let now = Instant::now();
        let elapsed = now.duration_since(self.last_refill).as_secs_f32();
        self.tokens = (self.tokens + elapsed * self.refill_rate).min(self.capacity);
        self.last_refill = now;

        if self.tokens >= 1.0 {
            self.tokens -= 1.0;
            true
        } else {
            false
        }
    }
}

// ---------------------------------------------------------------------------
// SearchQuery
// ---------------------------------------------------------------------------

/// A query for searching memory entries.
///
/// ```
/// use memory_store::search::SearchQuery;
///
/// let q = SearchQuery::default()
///     .with_query("user prefers Python")
///     .with_user("alice")
///     .with_limit(5);
/// ```
#[derive(Debug, Clone)]
pub struct SearchQuery {
    /// Natural language query text.
    pub query: String,
    /// Scope results to a specific user.
    pub user_id: Option<String>,
    /// Filter by memory types. Empty means all types.
    pub memory_types: Vec<MemoryType>,
    /// Filter by memory tiers. Empty means all tiers.
    pub tiers: Vec<MemoryTier>,
    /// Include archived (closed) issues in results.
    pub include_archived: bool,
    /// Maximum number of results to return.
    pub limit: usize,
}

impl Default for SearchQuery {
    fn default() -> Self {
        Self {
            query: String::new(),
            user_id: None,
            memory_types: Vec::new(),
            tiers: Vec::new(),
            include_archived: false,
            limit: 10,
        }
    }
}

impl SearchQuery {
    /// Sets the natural language query text.
    pub fn with_query(mut self, q: &str) -> Self {
        self.query = q.to_owned();
        self
    }

    /// Scopes the search to a specific user.
    pub fn with_user(mut self, id: &str) -> Self {
        self.user_id = Some(id.to_owned());
        self
    }

    /// Filters by memory types.
    pub fn with_memory_types(mut self, types: Vec<MemoryType>) -> Self {
        self.memory_types = types;
        self
    }

    /// Filters by memory tiers.
    pub fn with_tiers(mut self, tiers: Vec<MemoryTier>) -> Self {
        self.tiers = tiers;
        self
    }

    /// Includes archived (closed) issues in results.
    pub fn with_archived(mut self, include: bool) -> Self {
        self.include_archived = include;
        self
    }

    /// Sets the maximum number of results to return.
    pub fn with_limit(mut self, limit: usize) -> Self {
        self.limit = limit;
        self
    }
}

// ---------------------------------------------------------------------------
// SearchResult
// ---------------------------------------------------------------------------

/// A ranked search result.
#[derive(Debug, Clone)]
pub struct SearchResult {
    /// The parsed memory entry.
    pub entry: MemoryEntry,
    /// Local combined relevance score (0.0–1.0).
    pub score: f32,
    /// Raw GitHub relevance score.
    pub gh_score: f32,
}

// ---------------------------------------------------------------------------
// SearchEngine
// ---------------------------------------------------------------------------

/// Cache entry: (insertion time, results).
type CacheEntry = (Instant, Vec<SearchResult>);

/// The primary search driver for GitHub Issues-backed memory.
///
/// Supports hybrid search (semantic + keyword via `search_type=hybrid`) and
/// lexical search (keyword-only, no rate-limit cost). Hybrid results are cached
/// for 120 seconds to conserve the 10 req/min semantic quota.
pub struct SearchEngine {
    client: reqwest::Client,
    repo: String,
    token: String,
    /// Rate limiter for semantic/hybrid search (10 req/min).
    semantic_limiter: Arc<Mutex<TokenBucket>>,
    /// Query key → (inserted_at, results).
    cache: Arc<Mutex<HashMap<String, CacheEntry>>>,
}

impl SearchEngine {
    /// Creates a new `SearchEngine`.
    ///
    /// Token bucket: capacity=10, refill_rate=10/60 (matches GitHub's 10 req/min limit).
    pub fn new(repo: &str, token: &str) -> Self {
        let bucket = TokenBucket::new(10.0, 10.0 / 60.0);
        Self {
            client: reqwest::Client::builder()
                .user_agent("memory-store/0.1.0")
                .build()
                .expect("failed to build reqwest client"),
            repo: repo.to_owned(),
            token: token.to_owned(),
            semantic_limiter: Arc::new(Mutex::new(bucket)),
            cache: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    // -----------------------------------------------------------------------
    // Public API
    // -----------------------------------------------------------------------

    /// Performs a hybrid (semantic + keyword) search against GitHub Issues.
    ///
    /// Consumes one token from the rate-limit bucket (blocks until available).
    /// Results are cached for 120 seconds.
    pub async fn hybrid_search(&self, query: &SearchQuery) -> Result<Vec<SearchResult>> {
        let cache_key = Self::cache_key(query);

        // Check cache — lock dropped before any await.
        {
            let cache = self.cache.lock().await;
            if let Some((inserted, results)) = cache.get(&cache_key) {
                if inserted.elapsed() < Duration::from_secs(120) {
                    return Ok(results.clone());
                }
            }
        }

        // Acquire a token — never hold the Mutex across an await point.
        loop {
            let acquired = {
                let mut limiter = self.semantic_limiter.lock().await;
                limiter.try_acquire()
            };
            if acquired {
                break;
            }
            sleep(Duration::from_secs(1)).await;
        }

        let extra_params: &[(&str, &str)] = &[("search_type", "hybrid")];
        let results = self.execute_search(query, extra_params).await?;

        // Store in cache — lock dropped before returning.
        {
            let mut cache = self.cache.lock().await;
            cache.insert(cache_key, (Instant::now(), results.clone()));
        }

        Ok(results)
    }

    /// Performs a lexical (keyword-only) search against GitHub Issues.
    ///
    /// Does not consume a rate-limit token and does not cache results.
    pub async fn lexical_search(&self, query: &SearchQuery) -> Result<Vec<SearchResult>> {
        self.execute_search(query, &[]).await
    }

    /// Clears all cached search results.
    pub fn invalidate_cache(&self) {
        let cache = self.cache.clone();
        tokio::spawn(async move {
            cache.lock().await.clear();
        });
    }

    // -----------------------------------------------------------------------
    // Private helpers
    // -----------------------------------------------------------------------

    /// Shared HTTP execution for both hybrid and lexical search.
    ///
    /// `extra_params` are appended to the base query parameters (`q`, `per_page`, `sort`).
    async fn execute_search(
        &self,
        query: &SearchQuery,
        extra_params: &[(&str, &str)],
    ) -> Result<Vec<SearchResult>> {
        let gh_query = Self::build_gh_query(query, &self.repo);
        let per_page = (query.limit * 2).min(30).to_string();

        let mut params: Vec<(&str, &str)> = vec![
            ("q", &gh_query),
            ("per_page", &per_page),
            ("sort", "best-match"),
        ];
        params.extend_from_slice(extra_params);

        let response = self
            .client
            .get("https://api.github.com/search/issues")
            .bearer_auth(&self.token)
            .header("Accept", "application/vnd.github+json")
            .header("X-GitHub-Api-Version", "2022-11-28")
            .query(&params)
            .send()
            .await?;

        let status = response.status().as_u16();
        if status == 403 || status == 429 {
            return Err(MemoryError::RateLimit {
                retry_after_secs: 60,
            });
        }
        if !response.status().is_success() {
            let msg = response.text().await.unwrap_or_default();
            return Err(MemoryError::GithubApi {
                status,
                message: msg,
            });
        }

        let body: serde_json::Value = response.json().await?;
        let items = body["items"]
            .as_array()
            .map(|v| v.as_slice())
            .unwrap_or(&[]);
        let parsed = Self::parse_search_items(items);

        let mut results: Vec<SearchResult> = parsed
            .into_iter()
            .map(|(entry, gh_score)| {
                let score = Self::local_score(&entry, gh_score);
                SearchResult {
                    entry,
                    score,
                    gh_score,
                }
            })
            .collect();

        results.sort_by(|a, b| b.score.total_cmp(&a.score));
        results.truncate(query.limit);
        Ok(results)
    }

    /// Builds the GitHub Issues search query string from a [`SearchQuery`].
    ///
    /// Format: `repo:{repo} [state:open] [label:user:{id}] [label:{type}...] [label:{tier}...] {query}`
    fn build_gh_query(q: &SearchQuery, repo: &str) -> String {
        let mut parts: Vec<String> = Vec::new();

        parts.push(format!("repo:{repo}"));

        if !q.include_archived {
            parts.push("state:open".to_owned());
        }

        if let Some(uid) = &q.user_id {
            parts.push(format!("label:{}", crate::labels::user_label(uid)));
        }

        for mt in &q.memory_types {
            parts.push(format!("label:{}", crate::labels::type_label(mt)));
        }

        for tier in &q.tiers {
            parts.push(format!("label:{}", crate::labels::tier_label(tier)));
        }

        if !q.query.is_empty() {
            parts.push(q.query.clone());
        }

        parts.join(" ")
    }

    /// Computes the local combined relevance score.
    ///
    /// `0.5 × normalized_gh + 0.3 × importance + 0.2 × recency`
    ///
    /// - `normalized_gh = gh_score.min(100.0) / 100.0`
    /// - `recency = max(0.0, 1.0 - age_days / 30.0)` (age from `created_at`)
    fn local_score(entry: &MemoryEntry, gh_score: f32) -> f32 {
        let normalized_gh = gh_score.min(100.0) / 100.0;
        let age_days = (chrono::Utc::now() - entry.created_at).num_days() as f32;
        let recency = (1.0 - age_days / 30.0).max(0.0);
        0.5 * normalized_gh + 0.3 * entry.importance + 0.2 * recency
    }

    /// Builds a stable cache key from all query fields.
    fn cache_key(q: &SearchQuery) -> String {
        let types = q
            .memory_types
            .iter()
            .map(|t| t.to_string())
            .collect::<Vec<_>>()
            .join(",");
        let tiers = q
            .tiers
            .iter()
            .map(|t| t.to_string())
            .collect::<Vec<_>>()
            .join(",");
        format!(
            "{}|{}|{}|{}|{}|{}",
            q.query,
            q.user_id.as_deref().unwrap_or(""),
            types,
            tiers,
            q.include_archived,
            q.limit,
        )
    }

    /// Parses the `items` array from a GitHub Issues search response.
    ///
    /// Skips items that fail to parse so one bad issue doesn't abort the search.
    fn parse_search_items(items: &[serde_json::Value]) -> Vec<(MemoryEntry, f32)> {
        items
            .iter()
            .filter_map(|item| {
                let title = item["title"].as_str().unwrap_or("");
                let body = item["body"].as_str().unwrap_or("");
                let gh_score = item["score"].as_f64().unwrap_or(0.0) as f32;
                let issue_number = item["number"].as_u64();

                crate::schema::parse_body(title, body)
                    .ok()
                    .map(|mut entry| {
                        entry.issue_number = issue_number;
                        (entry, gh_score)
                    })
            })
            .collect()
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{MemoryTier, MemoryType};
    use chrono::Utc;
    use uuid::Uuid;

    // -----------------------------------------------------------------------
    // TokenBucket
    // -----------------------------------------------------------------------

    #[test]
    fn token_bucket_ten_immediate_acquisitions_succeed() {
        let mut bucket = TokenBucket::new(10.0, 10.0 / 60.0);
        for i in 0..10 {
            assert!(bucket.try_acquire(), "acquisition {i} should succeed");
        }
    }

    #[test]
    fn token_bucket_eleventh_acquisition_fails() {
        let mut bucket = TokenBucket::new(10.0, 10.0 / 60.0);
        for _ in 0..10 {
            bucket.try_acquire();
        }
        assert!(!bucket.try_acquire(), "11th acquisition should fail");
    }

    #[test]
    fn token_bucket_refills_after_elapsed_time() {
        let mut bucket = TokenBucket::new(10.0, 10.0 / 60.0);
        for _ in 0..10 {
            bucket.try_acquire();
        }
        assert!(!bucket.try_acquire());
        // 10/60 ≈ 0.167 tokens/sec → 7s yields ~1.17 tokens
        std::thread::sleep(Duration::from_secs(7));
        assert!(
            bucket.try_acquire(),
            "should have refilled at least one token after 7s"
        );
    }

    #[test]
    fn token_bucket_never_exceeds_capacity_after_long_idle() {
        let mut bucket = TokenBucket::new(10.0, 10.0 / 60.0);
        for _ in 0..10 {
            bucket.try_acquire();
        }
        // 65s > 60s full-refill period; capacity is 10, so we must get exactly 10.
        std::thread::sleep(Duration::from_secs(65));
        let mut count = 0;
        while bucket.try_acquire() {
            count += 1;
            if count > 11 {
                break; // safety guard
            }
        }
        assert_eq!(count, 10, "capacity=10 should be the ceiling, got {count}");
    }

    // -----------------------------------------------------------------------
    // build_gh_query
    // -----------------------------------------------------------------------

    #[test]
    fn build_gh_query_includes_repo() {
        let q = SearchQuery::default().with_query("test");
        let result = SearchEngine::build_gh_query(&q, "owner/repo");
        assert!(result.contains("repo:owner/repo"), "missing repo: {result}");
    }

    #[test]
    fn build_gh_query_includes_state_open_by_default() {
        let q = SearchQuery::default();
        let result = SearchEngine::build_gh_query(&q, "owner/repo");
        assert!(
            result.contains("state:open"),
            "missing state:open: {result}"
        );
    }

    #[test]
    fn build_gh_query_omits_state_open_when_include_archived() {
        let q = SearchQuery::default().with_archived(true);
        let result = SearchEngine::build_gh_query(&q, "owner/repo");
        assert!(
            !result.contains("state:open"),
            "should omit state:open: {result}"
        );
    }

    #[test]
    fn build_gh_query_with_user_id_includes_label() {
        let q = SearchQuery::default().with_user("alice");
        let result = SearchEngine::build_gh_query(&q, "owner/repo");
        assert!(
            result.contains("label:user:alice"),
            "missing label:user:alice: {result}"
        );
    }

    #[test]
    fn build_gh_query_with_memory_types_includes_type_label() {
        let q = SearchQuery::default().with_memory_types(vec![MemoryType::Semantic]);
        let result = SearchEngine::build_gh_query(&q, "owner/repo");
        assert!(
            result.contains("label:type:semantic"),
            "missing type label: {result}"
        );
    }

    #[test]
    fn build_gh_query_with_tiers_includes_tier_label() {
        let q = SearchQuery::default().with_tiers(vec![MemoryTier::Cold]);
        let result = SearchEngine::build_gh_query(&q, "owner/repo");
        assert!(
            result.contains("label:tier:cold"),
            "missing tier label: {result}"
        );
    }

    #[test]
    fn build_gh_query_appends_query_text_last() {
        let q = SearchQuery::default()
            .with_user("alice")
            .with_query("python preferences");
        let result = SearchEngine::build_gh_query(&q, "owner/repo");
        assert!(
            result.ends_with("python preferences"),
            "query text should be last: {result}"
        );
    }

    // -----------------------------------------------------------------------
    // local_score
    // -----------------------------------------------------------------------

    fn make_entry_with_age(days_old: i64, importance: f32) -> MemoryEntry {
        MemoryEntry {
            memory_id: Uuid::new_v4(),
            issue_number: None,
            content: "test".to_owned(),
            memory_type: MemoryType::Episodic,
            user_id: None,
            agent_id: None,
            session_id: None,
            importance,
            confidence: 0.8,
            access_count: 0,
            last_accessed: None,
            created_at: Utc::now() - chrono::Duration::days(days_old),
            entities: Vec::new(),
            tags: Vec::new(),
            structured_data: serde_json::Value::Object(Default::default()),
            supersedes: Vec::new(),
            related_to: Vec::new(),
        }
    }

    #[test]
    fn local_score_brand_new_high_importance_is_high() {
        let entry = make_entry_with_age(0, 0.9);
        let score = SearchEngine::local_score(&entry, 100.0);
        // 0.5×1.0 + 0.3×0.9 + 0.2×1.0 = 0.97
        assert!(
            score > 0.9,
            "expected high score for new+high-importance entry, got {score}"
        );
    }

    #[test]
    fn local_score_old_entry_lower_than_new() {
        let new_entry = make_entry_with_age(0, 0.5);
        let old_entry = make_entry_with_age(60, 0.5);
        let new_score = SearchEngine::local_score(&new_entry, 50.0);
        let old_score = SearchEngine::local_score(&old_entry, 50.0);
        assert!(
            new_score > old_score,
            "new entry ({new_score}) should score higher than 60-day-old ({old_score})"
        );
    }

    #[test]
    fn local_score_recency_clamped_at_zero_for_very_old() {
        let entry = make_entry_with_age(365, 0.0);
        let score = SearchEngine::local_score(&entry, 0.0);
        assert_eq!(score, 0.0, "score should be 0 for zero-everything entry");
    }

    #[test]
    fn local_score_normalized_gh_capped_at_100() {
        let entry = make_entry_with_age(0, 0.0);
        let score_100 = SearchEngine::local_score(&entry, 100.0);
        let score_200 = SearchEngine::local_score(&entry, 200.0);
        assert_eq!(
            score_100, score_200,
            "gh_score >100 should normalize same as 100"
        );
    }
}
