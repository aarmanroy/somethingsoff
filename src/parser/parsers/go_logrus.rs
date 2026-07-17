//! Go logrus text format parser
//!
//! Parses Go logrus output in text mode:
//! - Format: 2024/03/25 10:00:00 INFO file.go:123: message

use crate::parser::{log_format::LogFormat, LogParser};
use crate::schema::RawLogEntry;
use once_cell::sync::Lazy;
use regex::Regex;

/// Go logrus text format parser
pub struct GoLogrusParser;

/// Go logrus text format
/// Pattern: 2024/03/25 10:00:00 INFO file.go:123: message
/// Or: 2024/03/25 10:00:00 INFO message
static GO_LOGRUS_PATTERN: Lazy<Regex> = Lazy::new(|| {
    Regex::new(
        r"^(?P<timestamp>\d{4}/\d{2}/\d{2}\s+\d{2}:\d{2}:\d{2})\s+(?P<level>\w+)\s+(?:(?P<file>[\w.]+):\d+:\s+)?(?P<message>.*)",
    )
    .unwrap()
});

impl LogParser for GoLogrusParser {
    fn parse(&self, line: &str, source: &str) -> Option<RawLogEntry> {
        let caps = GO_LOGRUS_PATTERN.captures(line)?;

        Some(RawLogEntry {
            timestamp: Some(parse_go_timestamp(caps.name("timestamp")?.as_str())?),
            level: Some(normalize_level(caps.name("level")?.as_str())),
            message: Some(extract_message(caps.name("message")?.as_str())),
            source: Some(source.to_string()),
            source_file: caps
                .name("file")
                .map(|m: regex::Match| m.as_str().to_string()),
            ..Default::default()
        })
    }

    fn format_name(&self) -> &str {
        "go-logrus"
    }

    fn format(&self) -> LogFormat {
        LogFormat::GoLogrus
    }
}

/// Parse Go timestamp to ISO8601 format
/// Input: 2024/03/25 10:00:00
/// Output: 2024-03-25T10:00:00.000Z
fn parse_go_timestamp(ts: &str) -> Option<String> {
    let parts: Vec<&str> = ts.split_whitespace().collect::<Vec<_>>();
    if parts.len() != 2 {
        return None;
    }

    let date_part = parts[0].replace('/', "-");
    let time_part = parts[1];

    Some(format!("{}T{}.000Z", date_part, time_part))
}

/// Normalize Go log level to lowercase
fn normalize_level(level: &str) -> String {
    match level.to_uppercase().as_str() {
        "WARNING" => "warn".to_string(),
        "FATAL" | "PANIC" => "error".to_string(),
        other => other.to_lowercase(),
    }
}

/// Extract message, removing trailing newlines
fn extract_message(msg: &str) -> String {
    msg.trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_go_logrus_with_file() {
        let line = r#"2024/03/25 10:00:00 INFO main.go:45: Starting server"#;
        let parser = GoLogrusParser;
        let result = parser.parse(line, "go-app");

        assert!(result.is_some());
        let entry = result.unwrap();
        assert_eq!(entry.message, Some("Starting server".to_string()));
        assert_eq!(entry.level, Some("info".to_string()));
        assert_eq!(entry.source_file, Some("main.go".to_string()));
    }

    #[test]
    fn test_parse_go_logrus_without_file() {
        let line = r#"2024/03/25 10:00:00 ERROR Connection failed"#;
        let parser = GoLogrusParser;
        let result = parser.parse(line, "go-app");

        assert!(result.is_some());
        let entry = result.unwrap();
        assert_eq!(entry.message, Some("Connection failed".to_string()));
        assert_eq!(entry.level, Some("error".to_string()));
    }

    #[test]
    fn test_parse_go_logrus_warn_level() {
        let line = r#"2024/03/25 10:00:00 WARN db.go:123: Slow query"#;
        let parser = GoLogrusParser;
        let result = parser.parse(line, "go-app");

        assert!(result.is_some());
        let entry = result.unwrap();
        assert_eq!(entry.level, Some("warn".to_string()));
    }

    #[test]
    fn test_parse_go_logrus_fatal_level() {
        let line = r#"2024/03/25 10:00:00 FATAL app.go:10: Critical error"#;
        let parser = GoLogrusParser;
        let result = parser.parse(line, "go-app");

        assert!(result.is_some());
        let entry = result.unwrap();
        assert_eq!(entry.level, Some("error".to_string()));
    }

    #[test]
    fn test_parse_go_logrus_debug_level() {
        let line = r#"2024/03/25 10:00:00 DEBUG handler.go:50: Processing request"#;
        let parser = GoLogrusParser;
        let result = parser.parse(line, "go-app");

        assert!(result.is_some());
        let entry = result.unwrap();
        assert_eq!(entry.level, Some("debug".to_string()));
    }

    #[test]
    fn test_parse_non_go_logrus_returns_none() {
        let line = r#"{"json": "format"}"#;
        let parser = GoLogrusParser;
        let result = parser.parse(line, "go-app");

        assert!(result.is_none());
    }

    #[test]
    fn test_format_name() {
        assert_eq!(GoLogrusParser.format_name(), "go-logrus");
    }

    #[test]
    fn test_format() {
        assert_eq!(GoLogrusParser.format(), LogFormat::GoLogrus);
    }
}
