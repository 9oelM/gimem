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

pub mod consolidation;
pub mod error;
pub mod labels;
pub mod manager;
pub mod models;
pub mod schema;
pub mod search;
pub mod store;

pub use consolidation::{
    ConsolidationConfig, ConsolidationEngine, ConsolidationStats, EvictionStats, SummarizeFn,
};
pub use error::{MemoryError, Result};
pub use manager::MemoryManager;
pub use models::{MemoryEntry, MemoryEntryBuilder, MemoryPatch, MemoryTier, MemoryType};
pub use search::{SearchQuery, SearchResult};
pub use store::MemoryStore;
