//! Python logging format parser
//!
//! Parses Python logging module output in various formats:
//! - Default: 2024-03-25 10:00:00,000 - INFO - module - message
//! - Custom: 2024-03-25 10:00:00,123 | INFO | module | message

use crate::parser::{log_format::LogFormat, LogParser};
use crate::schema::RawLogEntry;
use once_cell::sync::Lazy;
use regex::Regex;

/// Python logging format parser
pub struct PythonParser;

/// Docs-canonical Python logging format
/// (`%(asctime)s - %(name)s - %(levelname)s - %(message)s`):
/// Pattern: 2024-03-25 10:00:00,000 - module - LEVEL - message
/// The level position is anchored to known level names so level-first
/// lines fall through to the STD pattern instead of mis-capturing.
static PYTHON_PATTERN_NAME_FIRST: Lazy<Regex> = Lazy::new(|| {
    Regex::new(
        r"^(?P<timestamp>\d{4}-\d{2}-\d{2}\s+\d{2}:\d{2}:\d{2}[,.]\d{3})\s+[-|]\s+(?P<module>[\w.:]+)\s+[-|]\s+(?P<level>(?i:DEBUG|INFO|WARN|WARNING|ERROR|CRITICAL|FATAL))\s+[-|]\s*(?P<message>.*)",
    )
    .unwrap()
});

/// Standard Python logging format
/// Pattern: 2024-03-25 10:00:00,000 - LEVEL - module - message
static PYTHON_PATTERN_STD: Lazy<Regex> = Lazy::new(|| {
    Regex::new(
        r"^(?P<timestamp>\d{4}-\d{2}-\d{2}\s+\d{2}:\d{2}:\d{2}[,.]\d{3})\s+[-|]\s+(?P<level>\w+)\s+[-|]\s+(?P<module>[\w.:]+)?\s+[-|]\s*(?P<message>.*)",
    )
    .unwrap()
});

/// Simplified Python format (no module)
/// Pattern: 2024-03-25 10:00:00,000 - LEVEL - message
static PYTHON_PATTERN_SIMPLE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(
        r"^(?P<timestamp>\d{4}-\d{2}-\d{2}\s+\d{2}:\d{2}:\d{2}[,.]\d{3})\s+[-|]\s+(?P<level>\w+)\s+[-|]\s*(?P<message>.*)",
    )
    .unwrap()
});

impl LogParser for PythonParser {
    fn parse(&self, line: &str, source: &str) -> Option<RawLogEntry> {
        // Try the docs-canonical name-first format before STD: its level
        // field is anchored to known level names, so it cannot mis-match a
        // level-first line, while STD would mis-capture a name-first one.
        if let Some(caps) = PYTHON_PATTERN_NAME_FIRST.captures(line) {
            let ts = caps.name("timestamp")?.as_str();
            let level = caps.name("level")?.as_str();
            let msg = caps.name("message")?.as_str();
            let module = caps
                .name("module")
                .map(|m: regex::Match| m.as_str().to_string());

            return Some(RawLogEntry {
                timestamp: Some(parse_python_timestamp(ts)?),
                level: Some(normalize_level(level)),
                message: Some(extract_message(msg)),
                source: Some(source.to_string()),
                source_file: module,
                ..Default::default()
            });
        }

        // Try standard format
        if let Some(caps) = PYTHON_PATTERN_STD.captures(line) {
            let ts = caps.name("timestamp")?.as_str();
            let level = caps.name("level")?.as_str();
            let msg = caps.name("message")?.as_str();
            let module = caps
                .name("module")
                .map(|m: regex::Match| m.as_str().to_string());

            return Some(RawLogEntry {
                timestamp: Some(parse_python_timestamp(ts)?),
                level: Some(normalize_level(level)),
                message: Some(extract_message(msg)),
                source: Some(source.to_string()),
                source_file: module,
                ..Default::default()
            });
        }

        // Try simplified format
        if let Some(caps) = PYTHON_PATTERN_SIMPLE.captures(line) {
            let ts = caps.name("timestamp")?.as_str();
            let level = caps.name("level")?.as_str();
            let msg = caps.name("message")?.as_str();

            return Some(RawLogEntry {
                timestamp: Some(parse_python_timestamp(ts)?),
                level: Some(normalize_level(level)),
                message: Some(extract_message(msg)),
                source: Some(source.to_string()),
                ..Default::default()
            });
        }

        None
    }

    fn format_name(&self) -> &str {
        "python-logging"
    }

    fn format(&self) -> LogFormat {
        LogFormat::PythonLogging
    }
}

/// Parse Python timestamp to ISO8601 format
/// Input: 2024-03-25 10:00:00,000 or 2024-03-25 10:00:00.000
/// Output: 2024-03-25T10:00:00.000Z
///
/// The regex guarantees the matched time already carries millis (`[,.]\d{3}`),
/// so we only swap the comma for a dot and join with `T…Z` — appending an extra
/// `.000` would produce a malformed `…000.000Z` that later fails timestamp
/// normalization and epoch-millis conversion (dropping the doc from time filters).
fn parse_python_timestamp(ts: &str) -> Option<String> {
    let normalized = ts.replace(',', ".");
    let parts: Vec<&str> = normalized.split_whitespace().collect::<Vec<_>>();
    if parts.len() != 2 {
        return None;
    }

    Some(format!("{}T{}Z", parts[0], parts[1]))
}

/// Normalize Python log level to lowercase
fn normalize_level(level: &str) -> String {
    match level.to_uppercase().as_str() {
        "WARNING" => "warn".to_string(),
        "CRITICAL" | "FATAL" => "error".to_string(),
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
    fn test_parse_python_standard_format() {
        let line = r#"2024-03-25 10:00:00,000 - INFO - app.main - Starting server"#;
        let parser = PythonParser;
        let result = parser.parse(line, "python-app");

        assert!(result.is_some());
        let entry = result.unwrap();
        assert_eq!(entry.message, Some("Starting server".to_string()));
        assert_eq!(entry.level, Some("info".to_string()));
        assert_eq!(entry.source_file, Some("app.main".to_string()));
    }

    #[test]
    fn test_parse_python_timestamp_is_canonical_not_double_millis() {
        // Regression: the comma-millis time must become a single-fraction
        // canonical timestamp, not `...000.000Z` (which fails normalization and
        // epoch-millis conversion, dropping the doc from --last filters).
        assert_eq!(
            parse_python_timestamp("2026-07-15 08:00:00,013").as_deref(),
            Some("2026-07-15T08:00:00.013Z")
        );
        // Dotted input is handled identically.
        assert_eq!(
            parse_python_timestamp("2026-07-15 08:00:00.500").as_deref(),
            Some("2026-07-15T08:00:00.500Z")
        );
        // And the parsed value must survive normalization + millis parsing.
        let ts = parse_python_timestamp("2026-07-15 08:00:00,013").unwrap();
        assert_eq!(
            crate::schema::normalize_timestamp(&ts).as_deref(),
            Some("2026-07-15T08:00:00.013Z")
        );
        assert!(crate::schema::parse_timestamp_millis(&ts).is_some());
    }

    #[test]
    fn test_parse_python_simple_format() {
        let line = r#"2024-03-25 10:00:00,000 - ERROR - Database connection failed"#;
        let parser = PythonParser;
        let result = parser.parse(line, "python-app");

        assert!(result.is_some());
        let entry = result.unwrap();
        assert_eq!(
            entry.message,
            Some("Database connection failed".to_string())
        );
        assert_eq!(entry.level, Some("error".to_string()));
    }

    #[test]
    fn test_parse_python_docs_canonical_name_first_format() {
        // The layout from the official Python logging docs:
        // "%(asctime)s - %(name)s - %(levelname)s - %(message)s"
        let line = r#"2026-07-15 09:10:00,123 - auth - ERROR - token expired for session"#;
        let parser = PythonParser;
        let result = parser.parse(line, "python-app");

        assert!(result.is_some());
        let entry = result.unwrap();
        assert_eq!(entry.level, Some("error".to_string()));
        assert_eq!(entry.source_file, Some("auth".to_string()));
        assert_eq!(entry.message, Some("token expired for session".to_string()));
    }

    #[test]
    fn test_parse_python_name_first_with_dotted_module() {
        let line = r#"2026-07-15 09:10:00,123 - app.services.db - WARNING - pool exhausted"#;
        let parser = PythonParser;
        let result = parser.parse(line, "python-app");

        assert!(result.is_some());
        let entry = result.unwrap();
        assert_eq!(entry.level, Some("warn".to_string()));
        assert_eq!(entry.source_file, Some("app.services.db".to_string()));
    }

    #[test]
    fn test_level_first_still_parses_level_correctly() {
        // Guard: the name-first pattern must not hijack level-first lines.
        let line = r#"2024-03-25 10:00:00,000 - INFO - app.main - Starting server"#;
        let parser = PythonParser;
        let entry = parser.parse(line, "python-app").unwrap();
        assert_eq!(entry.level, Some("info".to_string()));
        assert_eq!(entry.source_file, Some("app.main".to_string()));
    }

    #[test]
    fn test_parse_python_with_pipe_separator() {
        let line = r#"2024-03-25 10:00:00,123 | WARN | app | Retrying connection"#;
        let parser = PythonParser;
        let result = parser.parse(line, "python-app");

        assert!(result.is_some());
        let entry = result.unwrap();
        assert_eq!(entry.message, Some("Retrying connection".to_string()));
        assert_eq!(entry.level, Some("warn".to_string()));
    }

    #[test]
    fn test_parse_python_warning_normalized_to_warn() {
        let line = r#"2024-03-25 10:00:00,000 - WARNING - This is a warning"#;
        let parser = PythonParser;
        let result = parser.parse(line, "python-app");

        assert!(result.is_some());
        let entry = result.unwrap();
        assert_eq!(entry.level, Some("warn".to_string()));
    }

    #[test]
    fn test_parse_python_critical_normalized_to_error() {
        let line = r#"2024-03-25 10:00:00,000 - CRITICAL - System failure"#;
        let parser = PythonParser;
        let result = parser.parse(line, "python-app");

        assert!(result.is_some());
        let entry = result.unwrap();
        assert_eq!(entry.level, Some("error".to_string()));
    }

    #[test]
    fn test_parse_non_python_returns_none() {
        let line = r#"{"json": "format"}"#;
        let parser = PythonParser;
        let result = parser.parse(line, "python-app");

        assert!(result.is_none());
    }

    #[test]
    fn test_format_name() {
        assert_eq!(PythonParser.format_name(), "python-logging");
    }

    #[test]
    fn test_format() {
        assert_eq!(PythonParser.format(), LogFormat::PythonLogging);
    }
}
