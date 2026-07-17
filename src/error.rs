use thiserror::Error;

#[derive(Error, Debug)]
#[allow(dead_code)]
pub enum LogServiceError {
    #[error("Schema error: {0}")]
    Schema(String),

    #[error("Index error: {0}")]
    Index(String),

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("Search error: {0}")]
    Search(String),

    #[error("Validation error: {0}")]
    Validation(String),

    #[error("Config error: {0}")]
    Config(String),

    #[error("Index corrupted: {0}")]
    Corrupted(String),

    #[error("Permission denied: {0}")]
    Permission(String),

    #[error("Parse error: {0}")]
    Parse(String),
}

impl LogServiceError {
    pub fn exit_code(&self) -> i32 {
        self.code().exit_code()
    }

    /// Map onto the v1 output-contract error codes.
    pub fn code(&self) -> crate::output::ErrorCode {
        use crate::output::ErrorCode;
        match self {
            LogServiceError::Config(_) => ErrorCode::ConfigInvalid,
            LogServiceError::Validation(_) => ErrorCode::Usage,
            LogServiceError::Corrupted(_) => ErrorCode::IndexCorrupt,
            LogServiceError::Permission(_) => ErrorCode::PermissionDenied,
            LogServiceError::Io(_) => ErrorCode::IoError,
            LogServiceError::Parse(_) | LogServiceError::Json(_) => ErrorCode::ParseError,
            LogServiceError::Schema(_) | LogServiceError::Index(_) | LogServiceError::Search(_) => {
                ErrorCode::Internal
            }
        }
    }

    /// Actionable next step for the error envelope, when one exists.
    pub fn hint(&self) -> Option<String> {
        match self {
            LogServiceError::Config(_) => Some(
                "Check .somethingsoff/config.toml for syntax errors, or delete it to use zero-config defaults".to_string(),
            ),
            LogServiceError::Corrupted(_) => Some(
                "Run `somethingsoff index rebuild` to recreate the index from your log files".to_string(),
            ),
            LogServiceError::Permission(_) => Some(
                "Check file permissions on the .somethingsoff directory and your log files".to_string(),
            ),
            _ => None,
        }
    }
}
