//! JSON log parser
//!
//! Parses JSON-line formatted logs (one JSON object per line)

use crate::parser::{log_format::LogFormat, LogParser};
use crate::schema::RawLogEntry;
use serde_json::Value;

/// JSON Lines parser
///
/// Most efficient parser - directly deserializes JSON to RawLogEntry
pub struct JsonParser;

impl LogParser for JsonParser {
    fn parse(&self, line: &str, source: &str) -> Option<RawLogEntry> {
        // Skip non-JSON fast
        if !line.trim_start().starts_with('{') {
            return None;
        }

        // Parse once as Value so we can inspect nested stack traces.
        let value: Value = serde_json::from_str(line).ok()?;

        // Pull out any nested stack-trace string *before* consuming `value` for
        // field mapping. Most log lines have no `error.stack`/`stack` key, so
        // this allocates nothing and lets us move `value` into `from_value`
        // instead of deep-cloning the whole object on every JSON line.
        let stack = value
            .get("error")
            .and_then(|error| error.get("stack"))
            .and_then(Value::as_str)
            .or_else(|| value.get("stack").and_then(Value::as_str))
            .map(str::to_string);

        // Alias-aware mapping: camelCase keys, epoch times, etc. resolve to
        // core fields; everything unrecognized is preserved in `extra`.
        let mut raw = RawLogEntry::from_value(value)?;

        // Set source if not present
        if raw.source.is_none() {
            raw.source = Some(source.to_string());
        }

        // If no source location was supplied, try to derive it from a stack
        // trace (nested `stack`, or a multi-line `message` as a fallback).
        if raw.source_file.is_none() || raw.line_number.is_none() {
            let stack = stack.or_else(|| {
                raw.message
                    .as_deref()
                    .filter(|msg| msg.contains('\n'))
                    .map(str::to_string)
            });
            if let Some(frame) = stack
                .as_deref()
                .map(crate::parser::parse_stack_trace)
                .and_then(|frames| frames.into_iter().next())
            {
                raw.source_file.get_or_insert(frame.file_path);
                raw.line_number.get_or_insert(frame.line_number);
            }
        }

        Some(raw)
    }

    fn format_name(&self) -> &str {
        "json"
    }

    fn format(&self) -> LogFormat {
        LogFormat::Json
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_valid_json() {
        let line = r#"{"timestamp":"2024-03-25T10:00:00Z","level":"info","message":"test msg"}"#;
        let parser = JsonParser;
        let result = parser.parse(line, "test");

        assert!(result.is_some());
        let entry = result.unwrap();
        assert_eq!(entry.message, Some("test msg".to_string()));
        assert_eq!(entry.level, Some("info".to_string()));
    }

    #[test]
    fn test_parse_json_with_optional_fields() {
        let line = r#"{"timestamp":"2024-03-25T10:00:00Z","level":"error","message":"failed","request_id":"req-123","error":{"name":"Error","message":"failed"}}"#;
        let parser = JsonParser;
        let result = parser.parse(line, "test");

        assert!(result.is_some());
        let entry = result.unwrap();
        assert_eq!(entry.request_id, Some("req-123".to_string()));
        assert!(entry.error.is_some());
    }

    #[test]
    fn test_parse_non_json_returns_none() {
        let line = "this is not json";
        let parser = JsonParser;
        let result = parser.parse(line, "test");

        assert!(result.is_none());
    }

    #[test]
    fn test_parse_json_with_source_override() {
        let line = r#"{"timestamp":"2024-03-25T10:00:00Z","level":"info","message":"test"}"#;
        let parser = JsonParser;
        let result = parser.parse(line, "backend");

        assert!(result.is_some());
        let entry = result.unwrap();
        assert_eq!(entry.source, Some("backend".to_string()));
    }

    #[test]
    fn test_format_name() {
        assert_eq!(JsonParser.format_name(), "json");
    }

    #[test]
    fn test_format() {
        assert_eq!(JsonParser.format(), LogFormat::Json);
    }

    #[test]
    fn test_parse_json_derives_source_location_from_error_stack() {
        let line = r#"{"timestamp":"2024-03-25T10:00:00Z","level":"error","message":"failed","error":{"name":"TypeError","message":"boom","stack":"TypeError: boom\n    at login (src/auth/login.ts:47:15)\n    at router (src/router.ts:23:10)"}}"#;
        let parser = JsonParser;
        let result = parser.parse(line, "test");

        assert!(result.is_some());
        let entry = result.unwrap();
        assert_eq!(entry.source_file, Some("src/auth/login.ts".to_string()));
        assert_eq!(entry.line_number, Some(47));
    }

    #[test]
    fn test_parse_json_preserves_explicit_source_location_over_stack() {
        let line = r#"{"timestamp":"2024-03-25T10:00:00Z","level":"error","message":"failed","source_file":"src/explicit.ts","line_number":99,"error":{"stack":"TypeError: boom\n    at login (src/auth/login.ts:47:15)"}}"#;
        let parser = JsonParser;
        let result = parser.parse(line, "test");

        assert!(result.is_some());
        let entry = result.unwrap();
        assert_eq!(entry.source_file, Some("src/explicit.ts".to_string()));
        assert_eq!(entry.line_number, Some(99));
    }
}
