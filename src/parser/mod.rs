//! Log parser module
//!
//! Part of Phase 1: Discovery
//! - Multi-format log parsing
//! - Format detection
//! - Normalization to common LogEntry structure

pub mod coalesce;
pub mod detector;
pub mod json;
pub mod learn;
pub mod log_format;
pub mod parsers;
pub mod stacktrace;

pub use coalesce::{Block, LineCoalescer};
pub use detector::detect_format;
pub use json::JsonParser;
pub use learn::{suggest_patterns, PatternSuggestion};
pub use log_format::LogFormat;
pub use parsers::{parse_block, parse_log_entry, LogParser};
pub use stacktrace::{parse_stack_trace, StackFrame};

use crate::schema::RawLogEntry;

/// Parse a log line using format detection
///
/// Automatically detects format and parses the line into a RawLogEntry
pub fn parse_line_with_auto_detect(line: &str, default_source: &str) -> Option<RawLogEntry> {
    parse_log_entry(line, default_source)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_json_line() {
        let line = r#"{"timestamp":"2024-03-25T10:00:00Z","level":"info","message":"test"}"#;
        let result = parse_line_with_auto_detect(line, "test");

        assert!(result.is_some());
        let entry = result.unwrap();
        assert_eq!(entry.message, Some("test".to_string()));
        assert_eq!(entry.level, Some("info".to_string()));
    }

    #[test]
    fn test_parse_empty_line() {
        let result = parse_line_with_auto_detect("", "test");
        assert!(result.is_none());
    }
}
