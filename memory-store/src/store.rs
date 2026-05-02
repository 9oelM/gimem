//! `MemoryStore` trait, `GitHubIssuesStore` HTTP implementation, and
//! `MockMemoryStore` for unit testing.

use crate::error::{MemoryError, Result};
use crate::models::MemoryEntry;
use crate::{labels, schema};

// ---------------------------------------------------------------------------
// Trait
// ---------------------------------------------------------------------------

/// Core persistence interface for memory entries.
///
/// Implementations must be `Send + Sync` so they can be shared across async
/// tasks. The trait is object-safe; use `Arc<dyn MemoryStore>` to share an
/// instance.
#[async_trait::async_trait]
pub trait MemoryStore: Send + Sync {
    /// Persist a new memory entry, assigning its `issue_number` on success.
    async fn create(&self, entry: &mut MemoryEntry) -> Result<()>;

    /// Retrieve a memory entry by its GitHub issue number.
    ///
    /// Returns `Ok(None)` when the issue does not exist (404).
    async fn get(&self, issue_number: u64) -> Result<Option<MemoryEntry>>;

    /// Overwrite the stored content of an existing memory entry.
    async fn update(&self, entry: &MemoryEntry) -> Result<()>;

    /// Close the GitHub issue (archive it) and record the reason as a comment.
    async fn archive(&self, issue_number: u64, reason: &str) -> Result<()>;

    /// Hard-delete the issue via the GraphQL `deleteIssue` mutation.
    ///
    /// Requires an admin-scoped token. Use this only for GDPR erasure — prefer
    /// [`archive`] for normal eviction.
    async fn delete(&self, issue_number: u64) -> Result<()>;

    /// Append a comment to an existing issue.
    async fn add_comment(&self, issue_number: u64, body: &str) -> Result<()>;

    /// Replace the label set on an existing issue.
    async fn set_labels(&self, issue_number: u64, labels: &[String]) -> Result<()>;
}

// ---------------------------------------------------------------------------
// GitHubIssuesStore
// ---------------------------------------------------------------------------

/// GitHub Issues-backed implementation of [`MemoryStore`].
///
/// Every memory entry maps to one GitHub Issue in the configured repository.
/// The HTTP client is built once at construction time and reused across calls.
pub struct GitHubIssuesStore {
    client: reqwest::Client,
    /// `"owner/repo"` string identifying the target repository.
    repo: String,
    /// GitHub personal access token stored for potential client reconstruction.
    #[allow(dead_code)]
    token: String,
}

impl GitHubIssuesStore {
    /// Construct a new store.
    ///
    /// Builds a [`reqwest::Client`] with the required GitHub API headers
    /// pre-configured (Authorization, Accept, X-GitHub-Api-Version).
    pub fn new(repo: &str, token: &str) -> Self {
        use reqwest::header::{self, HeaderMap, HeaderValue};

        let mut headers = HeaderMap::new();
        headers.insert(
            header::AUTHORIZATION,
            HeaderValue::from_str(&format!("Bearer {token}")).expect("valid token header"),
        );
        headers.insert(
            header::ACCEPT,
            HeaderValue::from_static("application/vnd.github+json"),
        );
        headers.insert(
            "X-GitHub-Api-Version",
            HeaderValue::from_static("2022-11-28"),
        );
        headers.insert(
            header::USER_AGENT,
            HeaderValue::from_static("memory-store/0.1.0"),
        );

        let client = reqwest::Client::builder()
            .default_headers(headers)
            .build()
            .expect("failed to build HTTP client");

        Self {
            client,
            repo: repo.to_owned(),
            token: token.to_owned(),
        }
    }

    /// Build an absolute GitHub REST API URL from a path fragment.
    fn api_url(&self, path: &str) -> String {
        format!("https://api.github.com{path}")
    }

    /// The GitHub GraphQL endpoint.
    fn graphql_url() -> &'static str {
        "https://api.github.com/graphql"
    }

    /// Map a non-2xx response to [`MemoryError::GithubApi`].
    ///
    /// Attempts to parse the GitHub error body `{"message": "..."}`.
    async fn check_response(response: reqwest::Response) -> Result<reqwest::Response> {
        let status = response.status();
        if status.is_success() {
            return Ok(response);
        }
        let status_code = status.as_u16();
        let body: serde_json::Value = response
            .json()
            .await
            .unwrap_or_else(|_| serde_json::json!({"message": "unknown error"}));
        let message = body["message"]
            .as_str()
            .unwrap_or("unknown error")
            .to_owned();
        Err(MemoryError::GithubApi { status: status_code, message })
    }

    /// Ensure all given label names exist in the repository, creating any that
    /// are missing. This operation is idempotent.
    pub async fn ensure_labels(&self, labels: &[&str]) -> Result<()> {
        // Fetch existing labels (first page; assumes < 100 managed labels).
        let url = self.api_url(&format!("/repos/{}/labels?per_page=100", self.repo));
        let resp = self.client.get(&url).send().await?;
        let resp = Self::check_response(resp).await?;
        let existing: Vec<serde_json::Value> = resp.json().await?;
        let existing_names: std::collections::HashSet<String> = existing
            .iter()
            .filter_map(|l| l["name"].as_str().map(|s| s.to_owned()))
            .collect();

        for &label in labels {
            if existing_names.contains(label) {
                continue;
            }
            let url = self.api_url(&format!("/repos/{}/labels", self.repo));
            let body = serde_json::json!({ "name": label, "color": "ededed" });
            let resp = self.client.post(&url).json(&body).send().await?;
            // 422 = already exists (race) — treat as success.
            let status = resp.status();
            if !status.is_success() && status.as_u16() != 422 {
                Self::check_response(resp).await?;
            }
        }
        Ok(())
    }
}

#[async_trait::async_trait]
impl MemoryStore for GitHubIssuesStore {
    async fn create(&self, entry: &mut MemoryEntry) -> Result<()> {
        let url = self.api_url(&format!("/repos/{}/issues", self.repo));
        let title = schema::format_title(entry);
        let body = schema::format_body(entry);
        let issue_labels = labels::labels_for_entry(entry);

        let payload = serde_json::json!({
            "title": title,
            "body": body,
            "labels": issue_labels,
        });

        let resp = self.client.post(&url).json(&payload).send().await?;
        let resp = Self::check_response(resp).await?;
        let json: serde_json::Value = resp.json().await?;

        let number = json["number"]
            .as_u64()
            .ok_or_else(|| MemoryError::InvalidInput("GitHub response missing 'number'".into()))?;
        entry.issue_number = Some(number);
        Ok(())
    }

    async fn get(&self, issue_number: u64) -> Result<Option<MemoryEntry>> {
        let url = self.api_url(&format!("/repos/{}/issues/{}", self.repo, issue_number));
        let resp = self.client.get(&url).send().await?;

        if resp.status().as_u16() == 404 {
            return Ok(None);
        }
        let resp = Self::check_response(resp).await?;
        let json: serde_json::Value = resp.json().await?;

        let title = json["title"].as_str().unwrap_or("");
        let body = json["body"].as_str().unwrap_or("");
        let mut entry = schema::parse_body(title, body)?;
        entry.issue_number = Some(issue_number);
        Ok(Some(entry))
    }

    async fn update(&self, entry: &MemoryEntry) -> Result<()> {
        let n = entry.issue_number.ok_or_else(|| {
            MemoryError::InvalidInput("entry has no issue_number; call create first".into())
        })?;

        let url = self.api_url(&format!("/repos/{}/issues/{}", self.repo, n));
        let title = schema::format_title(entry);
        let body = schema::format_body(entry);
        let issue_labels = labels::labels_for_entry(entry);

        let payload = serde_json::json!({
            "title": title,
            "body": body,
            "labels": issue_labels,
        });

        let resp = self.client.patch(&url).json(&payload).send().await?;
        Self::check_response(resp).await?;
        Ok(())
    }

    async fn archive(&self, issue_number: u64, reason: &str) -> Result<()> {
        // 1. Close the issue.
        let url = self.api_url(&format!("/repos/{}/issues/{}", self.repo, issue_number));
        let payload = serde_json::json!({ "state": "closed" });
        let resp = self.client.patch(&url).json(&payload).send().await?;
        Self::check_response(resp).await?;

        // 2. Post an archival comment.
        self.add_comment(issue_number, &format!("Archived: {reason}")).await?;
        Ok(())
    }

    async fn delete(&self, issue_number: u64) -> Result<()> {
        // Step 1: fetch the node_id via REST.
        let url = self.api_url(&format!("/repos/{}/issues/{}", self.repo, issue_number));
        let resp = self.client.get(&url).send().await?;
        let resp = Self::check_response(resp).await?;
        let json: serde_json::Value = resp.json().await?;

        let node_id = json["node_id"]
            .as_str()
            .ok_or_else(|| MemoryError::InvalidInput("GitHub response missing 'node_id'".into()))?
            .to_owned();

        // Step 2: delete via GraphQL.
        let mutation = r#"
            mutation DeleteIssue($issueId: ID!) {
                deleteIssue(input: { issueId: $issueId }) {
                    repository { nameWithOwner }
                }
            }
        "#;
        let payload = serde_json::json!({
            "query": mutation,
            "variables": { "issueId": node_id },
        });

        let resp = self
            .client
            .post(Self::graphql_url())
            .json(&payload)
            .send()
            .await?;
        let resp = Self::check_response(resp).await?;
        let body: serde_json::Value = resp.json().await?;

        // GraphQL returns errors in the body even for 200 responses.
        if let Some(errors) = body.get("errors") {
            let msg = errors
                .as_array()
                .and_then(|a| a.first())
                .and_then(|e| e["message"].as_str())
                .unwrap_or("GraphQL error")
                .to_owned();
            return Err(MemoryError::GithubApi { status: 200, message: msg });
        }
        Ok(())
    }

    async fn add_comment(&self, issue_number: u64, body: &str) -> Result<()> {
        let url =
            self.api_url(&format!("/repos/{}/issues/{}/comments", self.repo, issue_number));
        let payload = serde_json::json!({ "body": body });
        let resp = self.client.post(&url).json(&payload).send().await?;
        Self::check_response(resp).await?;
        Ok(())
    }

    async fn set_labels(&self, issue_number: u64, labels: &[String]) -> Result<()> {
        let url =
            self.api_url(&format!("/repos/{}/issues/{}/labels", self.repo, issue_number));
        let payload = serde_json::json!({ "labels": labels });
        let resp = self.client.put(&url).json(&payload).send().await?;
        Self::check_response(resp).await?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// testutil — MockMemoryStore
// ---------------------------------------------------------------------------

/// In-memory mock store for unit tests.
///
/// Exposed as `pub mod testutil` so integration tests and other crate consumers
/// can import it without enabling test configuration.
pub mod testutil {
    use super::*;
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::Arc;
    use tokio::sync::Mutex;

    /// Thread-safe in-memory implementation of [`MemoryStore`].
    ///
    /// Issues are numbered starting from 1 and never reused.
    pub struct MockMemoryStore {
        entries: Arc<Mutex<HashMap<u64, MemoryEntry>>>,
        archived: Arc<Mutex<HashMap<u64, String>>>,
        comments: Arc<Mutex<HashMap<u64, Vec<String>>>>,
        labels_map: Arc<Mutex<HashMap<u64, Vec<String>>>>,
        counter: AtomicU64,
    }

    impl MockMemoryStore {
        /// Create a new empty mock store.
        pub fn new() -> Self {
            Self {
                entries: Arc::new(Mutex::new(HashMap::new())),
                archived: Arc::new(Mutex::new(HashMap::new())),
                comments: Arc::new(Mutex::new(HashMap::new())),
                labels_map: Arc::new(Mutex::new(HashMap::new())),
                counter: AtomicU64::new(1),
            }
        }

        /// Returns `true` if the issue has been archived.
        pub async fn is_archived(&self, issue_number: u64) -> bool {
            self.archived.lock().await.contains_key(&issue_number)
        }

        /// Returns all comments posted to an issue.
        pub async fn comments_for(&self, issue_number: u64) -> Vec<String> {
            self.comments
                .lock()
                .await
                .get(&issue_number)
                .cloned()
                .unwrap_or_default()
        }

        /// Returns the labels currently set on an issue.
        pub async fn labels_for(&self, issue_number: u64) -> Vec<String> {
            self.labels_map
                .lock()
                .await
                .get(&issue_number)
                .cloned()
                .unwrap_or_default()
        }
    }

    impl Default for MockMemoryStore {
        fn default() -> Self {
            Self::new()
        }
    }

    #[async_trait::async_trait]
    impl MemoryStore for MockMemoryStore {
        async fn create(&self, entry: &mut MemoryEntry) -> Result<()> {
            let n = self.counter.fetch_add(1, Ordering::Relaxed);
            entry.issue_number = Some(n);
            self.entries.lock().await.insert(n, entry.clone());
            Ok(())
        }

        async fn get(&self, issue_number: u64) -> Result<Option<MemoryEntry>> {
            let map = self.entries.lock().await;
            Ok(map.get(&issue_number).cloned())
        }

        async fn update(&self, entry: &MemoryEntry) -> Result<()> {
            let n = entry.issue_number.ok_or_else(|| {
                MemoryError::InvalidInput(
                    "entry has no issue_number; call create first".into(),
                )
            })?;
            let mut map = self.entries.lock().await;
            map.insert(n, entry.clone());
            Ok(())
        }

        async fn archive(&self, issue_number: u64, reason: &str) -> Result<()> {
            {
                let mut archived = self.archived.lock().await;
                archived.insert(issue_number, reason.to_owned());
            }
            Ok(())
        }

        async fn delete(&self, issue_number: u64) -> Result<()> {
            let mut map = self.entries.lock().await;
            map.remove(&issue_number);
            Ok(())
        }

        async fn add_comment(&self, issue_number: u64, body: &str) -> Result<()> {
            let mut comments = self.comments.lock().await;
            comments
                .entry(issue_number)
                .or_default()
                .push(body.to_owned());
            Ok(())
        }

        async fn set_labels(&self, issue_number: u64, labels: &[String]) -> Result<()> {
            let mut map = self.labels_map.lock().await;
            map.insert(issue_number, labels.to_vec());
            Ok(())
        }
    }
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::testutil::MockMemoryStore;
    use super::*;
    use crate::models::{MemoryEntry, MemoryType};
    use chrono::Utc;
    use uuid::Uuid;

    fn make_entry() -> MemoryEntry {
        MemoryEntry {
            memory_id: Uuid::new_v4(),
            issue_number: None,
            content: "The user prefers Rust over C++.".to_owned(),
            memory_type: MemoryType::Semantic,
            user_id: Some("alice".to_owned()),
            agent_id: None,
            session_id: None,
            importance: 0.8,
            confidence: 0.9,
            access_count: 0,
            last_accessed: None,
            created_at: Utc::now(),
            entities: vec!["Rust".to_owned(), "C++".to_owned()],
            tags: vec!["preferences".to_owned()],
            structured_data: serde_json::Value::Object(Default::default()),
            supersedes: vec![],
            related_to: vec![],
        }
    }

    #[tokio::test]
    async fn create_assigns_issue_number() {
        let store = MockMemoryStore::new();
        let mut entry = make_entry();
        assert!(entry.issue_number.is_none());
        store.create(&mut entry).await.unwrap();
        assert!(entry.issue_number.is_some(), "issue_number should be set after create");
    }

    #[tokio::test]
    async fn get_returns_none_for_nonexistent() {
        let store = MockMemoryStore::new();
        let result = store.get(9999).await.unwrap();
        assert!(result.is_none(), "should return None for unknown issue");
    }

    #[tokio::test]
    async fn get_returns_stored_entry_after_create() {
        let store = MockMemoryStore::new();
        let mut entry = make_entry();
        store.create(&mut entry).await.unwrap();
        let n = entry.issue_number.unwrap();
        let retrieved = store.get(n).await.unwrap();
        assert!(retrieved.is_some(), "should return stored entry");
        assert_eq!(retrieved.unwrap().memory_id, entry.memory_id);
    }

    #[tokio::test]
    async fn update_reflects_changes_on_get() {
        let store = MockMemoryStore::new();
        let mut entry = make_entry();
        store.create(&mut entry).await.unwrap();
        let n = entry.issue_number.unwrap();

        entry.content = "Updated content.".to_owned();
        store.update(&entry).await.unwrap();

        let retrieved = store.get(n).await.unwrap().unwrap();
        assert_eq!(retrieved.content, "Updated content.");
    }

    #[tokio::test]
    async fn archive_marks_issue_as_archived() {
        let store = MockMemoryStore::new();
        let mut entry = make_entry();
        store.create(&mut entry).await.unwrap();
        let n = entry.issue_number.unwrap();

        store.archive(n, "evicted: low score").await.unwrap();
        assert!(store.is_archived(n).await, "issue should be archived");
    }

    #[tokio::test]
    async fn delete_removes_entry() {
        let store = MockMemoryStore::new();
        let mut entry = make_entry();
        store.create(&mut entry).await.unwrap();
        let n = entry.issue_number.unwrap();

        store.delete(n).await.unwrap();
        let result = store.get(n).await.unwrap();
        assert!(result.is_none(), "get after delete should return None");
    }

    #[tokio::test]
    async fn add_comment_does_not_error() {
        let store = MockMemoryStore::new();
        let mut entry = make_entry();
        store.create(&mut entry).await.unwrap();
        let n = entry.issue_number.unwrap();

        store.add_comment(n, "Access event logged.").await.unwrap();
        let comments = store.comments_for(n).await;
        assert!(!comments.is_empty(), "comment should be stored");
    }

    #[tokio::test]
    async fn set_labels_updates_labels() {
        let store = MockMemoryStore::new();
        let mut entry = make_entry();
        store.create(&mut entry).await.unwrap();
        let n = entry.issue_number.unwrap();

        let new_labels = vec!["type:semantic".to_owned(), "tier:warm".to_owned()];
        store.set_labels(n, &new_labels).await.unwrap();

        let stored = store.labels_for(n).await;
        assert_eq!(stored, new_labels);
    }

    #[tokio::test]
    async fn create_assigns_monotonically_increasing_numbers() {
        let store = MockMemoryStore::new();
        let mut e1 = make_entry();
        let mut e2 = make_entry();
        store.create(&mut e1).await.unwrap();
        store.create(&mut e2).await.unwrap();
        let n1 = e1.issue_number.unwrap();
        let n2 = e2.issue_number.unwrap();
        assert!(n2 > n1, "issue numbers should be monotonically increasing");
    }
}
