//! # memory-store
//!
//! A GitHub Issues-backed agent memory system.
//! GitHub Issues is the sole backend — no external database, no vector store.
//! GitHub's hybrid search API provides semantic retrieval.
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

pub mod error;
pub mod models;
pub mod labels;
pub mod schema;
pub mod store;
pub mod search;
pub mod consolidation;
pub mod manager;

pub use error::{MemoryError, Result};
pub use models::{MemoryEntry, MemoryEntryBuilder, MemoryType, MemoryTier, MemoryPatch};
pub use store::MemoryStore;
pub use search::{SearchQuery, SearchResult};
pub use consolidation::{ConsolidationConfig, ConsolidationEngine, ConsolidationStats, EvictionStats, SummarizeFn};
pub use manager::MemoryManager;
