//! Log4j/Log4j2 format parser
//!
//! Parses Java Log4j and Log4j2 output:
//! - Format: 2024-03-25 10:00:00,000 INFO  [thread] LoggerName - message
//! - Also supports: 2024-03-25 10:00:00.000 INFO LoggerName - message
//!
//! The fractional second is optional and variable-precision: `%d{yyyy-MM-dd
//! HH:mm:ss}` (no millis) is a common layout, and Java can emit micro- or
//! nanosecond precision. Requiring exactly three digits silently dropped both
//! shapes down to the raw fallback, which cost them their timestamp entirely.

use crate::parser::{log_format::LogFormat, LogParser};
use crate::schema::RawLogEntry;
use once_cell::sync::Lazy;
use regex::Regex;

/// Log4j/Log4j2 format parser
pub struct Log4jParser;

/// Log4j pattern with thread name
/// Pattern: 2024-03-25 10:00:00,000 INFO  [thread] LoggerName - message
static LOG4J_PATTERN_WITH_THREAD: Lazy<Regex> = Lazy::new(|| {
    Regex::new(
        r"^(?P<timestamp>\d{4}-\d{2}-\d{2}\s+\d{2}:\d{2}:\d{2}(?:[,.]\d{1,9})?)\s+(?P<level>\w+)\s+\[\s*(?P<thread>[\w\-]+)\s*\]\s+(?P<logger>[\w.]+)\s+-\s+(?P<message>.*)",
    )
    .unwrap()
});

/// Log4j pattern without thread name
/// Pattern: 2024-03-25 10:00:00,000 INFO LoggerName - message
static LOG4J_PATTERN_SIMPLE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(
        r"^(?P<timestamp>\d{4}-\d{2}-\d{2}\s+\d{2}:\d{2}:\d{2}(?:[,.]\d{1,9})?)\s+(?P<level>\w+)\s+(?P<logger>[\w.]+)\s+-\s+(?P<message>.*)",
    )
    .unwrap()
});

/// Log4j pattern without logger name
/// Pattern: 2024-03-25 10:00:00,000 INFO - message
static LOG4J_PATTERN_MINIMAL: Lazy<Regex> = Lazy::new(|| {
    Regex::new(
        r"^(?P<timestamp>\d{4}-\d{2}-\d{2}\s+\d{2}:\d{2}:\d{2}(?:[,.]\d{1,9})?)\s+(?P<level>\w+)\s+-\s+(?P<message>.*)",
    )
    .unwrap()
});

impl LogParser for Log4jParser {
    fn parse(&self, line: &str, source: &str) -> Option<RawLogEntry> {
        // Try pattern with thread name first
        if let Some(caps) = LOG4J_PATTERN_WITH_THREAD.captures(line) {
            let ts = caps.name("timestamp")?.as_str();
            let level = caps.name("level")?.as_str();
            let msg = caps.name("message")?.as_str();
            let logger = caps.name("logger")?.as_str();

            return Some(RawLogEntry {
                timestamp: Some(parse_log4j_timestamp(ts)?),
                level: Some(normalize_level(level)),
                message: Some(extract_message(msg)),
                source: Some(source.to_string()),
                source_file: Some(logger.to_string()),
                ..Default::default()
            });
        }

        // Try pattern without thread name
        if let Some(caps) = LOG4J_PATTERN_SIMPLE.captures(line) {
            let ts = caps.name("timestamp")?.as_str();
            let level = caps.name("level")?.as_str();
            let msg = caps.name("message")?.as_str();
            let logger = caps.name("logger")?.as_str();

            return Some(RawLogEntry {
                timestamp: Some(parse_log4j_timestamp(ts)?),
                level: Some(normalize_level(level)),
                message: Some(extract_message(msg)),
                source: Some(source.to_string()),
                source_file: Some(logger.to_string()),
                ..Default::default()
            });
        }

        // Try minimal pattern
        if let Some(caps) = LOG4J_PATTERN_MINIMAL.captures(line) {
            let ts = caps.name("timestamp")?.as_str();
            let level = caps.name("level")?.as_str();
            let msg = caps.name("message")?.as_str();

            return Some(RawLogEntry {
                timestamp: Some(parse_log4j_timestamp(ts)?),
                level: Some(normalize_level(level)),
                message: Some(extract_message(msg)),
                source: Some(source.to_string()),
                ..Default::default()
            });
        }

        None
    }

    fn format_name(&self) -> &str {
        "log4j"
    }

    fn format(&self) -> LogFormat {
        LogFormat::Log4j
    }
}

/// Parse Log4j timestamp to ISO8601 format
/// Input: 2024-03-25 10:00:00,000 or 2024-03-25 10:00:00.000
/// Output: 2024-03-25T10:00:00.000Z
fn parse_log4j_timestamp(ts: &str) -> Option<String> {
    let normalized = ts.replace(',', ".");
    let parts: Vec<&str> = normalized.split_whitespace().collect::<Vec<_>>();
    if parts.len() != 2 {
        return None;
    }
    let (date, time) = (parts[0], parts[1]);

    // Append millis only when the time doesn't already carry them. Appending
    // unconditionally produced `10:00:01.500.000Z`, which `normalize_timestamp`
    // rejects — so every log4j line written with the default layout (which does
    // include millis) got no `timestamp_ms` and was invisible to `--last`,
    // `--start` and `--end`. `python.rs` already guards this; log4j did not.
    if time.contains('.') {
        Some(format!("{date}T{time}Z"))
    } else {
        Some(format!("{date}T{time}.000Z"))
    }
}

/// Normalize Log4j log level to lowercase
fn normalize_level(level: &str) -> String {
    match level.to_uppercase().as_str() {
        "TRACE" => "debug".to_string(),
        "FATAL" => "error".to_string(),
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
    fn test_parse_log4j_with_thread() {
        let line = r#"2024-03-25 10:00:00,000 INFO  [main] com.example.App - Starting server"#;
        let parser = Log4jParser;
        let result = parser.parse(line, "java-app");

        assert!(result.is_some());
        let entry = result.unwrap();
        assert_eq!(entry.message, Some("Starting server".to_string()));
        assert_eq!(entry.level, Some("info".to_string()));
        assert_eq!(entry.source_file, Some("com.example.App".to_string()));
    }

    #[test]
    fn test_parse_log4j_without_thread() {
        let line = r#"2024-03-25 10:00:00,000 INFO com.example.Database - Connecting to DB"#;
        let parser = Log4jParser;
        let result = parser.parse(line, "java-app");

        assert!(result.is_some());
        let entry = result.unwrap();
        assert_eq!(entry.message, Some("Connecting to DB".to_string()));
        assert_eq!(entry.level, Some("info".to_string()));
    }

    #[test]
    fn test_parse_log4j_minimal() {
        let line = r#"2024-03-25 10:00:00,000 ERROR - Database connection failed"#;
        let parser = Log4jParser;
        let result = parser.parse(line, "java-app");

        assert!(result.is_some());
        let entry = result.unwrap();
        assert_eq!(
            entry.message,
            Some("Database connection failed".to_string())
        );
        assert_eq!(entry.level, Some("error".to_string()));
    }

    #[test]
    fn test_parse_log4j_trace_level() {
        let line = r#"2024-03-25 10:00:00,000 TRACE com.example.Debug - Detailed trace"#;
        let parser = Log4jParser;
        let result = parser.parse(line, "java-app");

        assert!(result.is_some());
        let entry = result.unwrap();
        assert_eq!(entry.level, Some("debug".to_string()));
    }

    #[test]
    fn test_parse_log4j_fatal_level() {
        let line = r#"2024-03-25 10:00:00,000 FATAL com.example.Critical - System shutdown"#;
        let parser = Log4jParser;
        let result = parser.parse(line, "java-app");

        assert!(result.is_some());
        let entry = result.unwrap();
        assert_eq!(entry.level, Some("error".to_string()));
    }

    #[test]
    fn test_parse_log4j_with_period_separator() {
        let line = r#"2024-03-25 10:00:00.123 INFO [main] com.example.App - Starting"#;
        let parser = Log4jParser;
        let result = parser.parse(line, "java-app");

        assert!(result.is_some());
        let entry = result.unwrap();
        assert_eq!(entry.message, Some("Starting".to_string()));
    }

    #[test]
    fn test_parse_log4j_multiline_message() {
        let line = r#"2024-03-25 10:00:00,000 INFO com.example.App - Starting server on port 8080"#;
        let parser = Log4jParser;
        let result = parser.parse(line, "java-app");

        assert!(result.is_some());
        let entry = result.unwrap();
        assert!(entry.message.as_ref().unwrap().contains("port 8080"));
    }

    #[test]
    fn test_parse_non_log4j_returns_none() {
        let line = r#"{"json": "format"}"#;
        let parser = Log4jParser;
        let result = parser.parse(line, "java-app");

        assert!(result.is_none());
    }

    #[test]
    fn test_format_name() {
        assert_eq!(Log4jParser.format_name(), "log4j");
    }

    #[test]
    fn test_format() {
        assert_eq!(Log4jParser.format(), LogFormat::Log4j);
    }
}
