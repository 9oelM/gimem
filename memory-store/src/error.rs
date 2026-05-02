//! Error types for the memory-store crate.

use thiserror::Error;

/// All errors that can occur in the memory-store library.
#[derive(Debug, Error)]
pub enum MemoryError {
    /// A GitHub API call returned a non-2xx HTTP status code.
    #[error("GitHub API error {status}: {message}")]
    GithubApi {
        /// HTTP status code returned by the GitHub API.
        status: u16,
        /// Human-readable error message from GitHub.
        message: String,
    },

    /// The semantic/hybrid search rate limit was hit.
    #[error("Rate limited — retry after {retry_after_secs}s")]
    RateLimit {
        /// Suggested number of seconds to wait before retrying.
        retry_after_secs: u64,
    },

    /// JSON parsing failed.
    #[error("Parse error: {0}")]
    Parse(#[from] serde_json::Error),

    /// An underlying HTTP request failed.
    #[error("HTTP error: {0}")]
    Http(#[from] reqwest::Error),

    /// A requested memory entry was not found.
    #[error("Memory entry not found: issue #{issue_number}")]
    NotFound {
        /// GitHub issue number of the missing entry.
        issue_number: u64,
    },

    /// The caller provided invalid input.
    #[error("Invalid input: {0}")]
    InvalidInput(String),
}

/// Convenience `Result` type aliased to [`MemoryError`].
pub type Result<T> = std::result::Result<T, MemoryError>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn github_api_error_display() {
        let err = MemoryError::GithubApi {
            status: 422,
            message: "Validation failed".to_string(),
        };
        let msg = err.to_string();
        assert!(msg.contains("422"), "status code missing: {msg}");
        assert!(msg.contains("Validation failed"), "message missing: {msg}");
    }

    #[test]
    fn rate_limit_display() {
        let err = MemoryError::RateLimit { retry_after_secs: 30 };
        let msg = err.to_string();
        assert!(msg.contains("30"), "seconds missing: {msg}");
    }

    #[test]
    fn not_found_display() {
        let err = MemoryError::NotFound { issue_number: 42 };
        let msg = err.to_string();
        assert!(msg.contains("42"), "issue number missing: {msg}");
    }

    #[test]
    fn invalid_input_display() {
        let err = MemoryError::InvalidInput("content must not be empty".to_string());
        let msg = err.to_string();
        assert!(msg.contains("content must not be empty"), "inner message missing: {msg}");
    }

    #[test]
    fn from_serde_json_error() {
        let json_err: serde_json::Error = serde_json::from_str::<serde_json::Value>("{bad}").unwrap_err();
        let mem_err: MemoryError = json_err.into();
        assert!(matches!(mem_err, MemoryError::Parse(_)));
    }

    #[test]
    fn parse_error_has_source() {
        use std::error::Error;
        let json_err: serde_json::Error = serde_json::from_str::<serde_json::Value>("{bad}").unwrap_err();
        let mem_err: MemoryError = json_err.into();
        assert!(mem_err.source().is_some(), "Parse error should chain source");
    }

    #[test]
    fn result_type_alias_works() {
        fn ok_result() -> Result<u32> {
            Ok(42)
        }
        fn err_result() -> Result<u32> {
            Err(MemoryError::InvalidInput("test".to_string()))
        }
        assert_eq!(ok_result().unwrap(), 42);
        assert!(err_result().is_err());
    }
}
