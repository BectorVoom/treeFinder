use serde::{Deserialize, Serialize};

/// Stable application error codes shared by CLI, MCP, and services.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ErrorCode {
    NotFound,
    InvalidPath,
    InvalidMarkdown,
    RevisionConflict,
    PatchFailed,
    IndexStale,
    IndexFailed,
    StrategyNotFound,
    LimitExceeded,
    PermissionDenied,
    AlreadyExists,
    InvalidArgument,
    Internal,
}

impl ErrorCode {
    pub fn as_str(self) -> &'static str {
        match self {
            ErrorCode::NotFound => "NOT_FOUND",
            ErrorCode::InvalidPath => "INVALID_PATH",
            ErrorCode::InvalidMarkdown => "INVALID_MARKDOWN",
            ErrorCode::RevisionConflict => "REVISION_CONFLICT",
            ErrorCode::PatchFailed => "PATCH_FAILED",
            ErrorCode::IndexStale => "INDEX_STALE",
            ErrorCode::IndexFailed => "INDEX_FAILED",
            ErrorCode::StrategyNotFound => "STRATEGY_NOT_FOUND",
            ErrorCode::LimitExceeded => "LIMIT_EXCEEDED",
            ErrorCode::PermissionDenied => "PERMISSION_DENIED",
            ErrorCode::AlreadyExists => "ALREADY_EXISTS",
            ErrorCode::InvalidArgument => "INVALID_ARGUMENT",
            ErrorCode::Internal => "INTERNAL",
        }
    }

    pub fn retryable(self) -> bool {
        matches!(self, ErrorCode::IndexStale)
    }

    /// CLI exit code mapping (spec §12).
    pub fn exit_code(self) -> i32 {
        match self {
            ErrorCode::NotFound | ErrorCode::StrategyNotFound => 3,
            ErrorCode::RevisionConflict | ErrorCode::AlreadyExists => 4,
            ErrorCode::InvalidPath
            | ErrorCode::InvalidMarkdown
            | ErrorCode::PatchFailed
            | ErrorCode::LimitExceeded
            | ErrorCode::InvalidArgument => 5,
            _ => 1,
        }
    }
}

#[derive(Debug, Clone, thiserror::Error)]
#[error("{code:?}: {message}")]
pub struct HdsError {
    pub code: ErrorCode,
    pub message: String,
    pub details: serde_json::Value,
}

pub type HdsResult<T> = Result<T, HdsError>;

impl HdsError {
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        HdsError {
            code,
            message: message.into(),
            details: serde_json::Value::Null,
        }
    }

    pub fn with_details(mut self, details: serde_json::Value) -> Self {
        self.details = details;
        self
    }

    pub fn not_found(what: impl std::fmt::Display) -> Self {
        Self::new(ErrorCode::NotFound, format!("{what} was not found"))
    }

    pub fn invalid_path(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::InvalidPath, message)
    }

    pub fn internal(message: impl std::fmt::Display) -> Self {
        Self::new(ErrorCode::Internal, message.to_string())
    }

    /// Wire form used by MCP tool errors and `--json` CLI errors (spec §11.5).
    pub fn to_wire(&self) -> serde_json::Value {
        serde_json::json!({
            "code": self.code.as_str(),
            "message": self.message,
            "details": self.details,
            "retryable": self.code.retryable(),
        })
    }
}

impl From<rusqlite::Error> for HdsError {
    fn from(e: rusqlite::Error) -> Self {
        HdsError::internal(format!("database error: {e}"))
    }
}

impl From<std::io::Error> for HdsError {
    fn from(e: std::io::Error) -> Self {
        HdsError::internal(format!("io error: {e}"))
    }
}

impl From<serde_json::Error> for HdsError {
    fn from(e: serde_json::Error) -> Self {
        HdsError::internal(format!("serialization error: {e}"))
    }
}
