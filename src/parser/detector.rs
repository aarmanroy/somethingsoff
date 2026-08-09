//! Log format detection module
//!
//! Part of Phase 1: Discovery
//! - Auto-detect log format from sample lines
//! - Supports JSON, Python logging, Go logrus, Apache/Nginx, Log4j
//! - Uses fast heuristics, not full parsing

use crate::parser::log_format::LogFormat;
use once_cell::sync::Lazy;
use regex::Regex;

/// Detect log format from sample lines
///
/// Uses fast heuristics to determine the most likely format.
/// Priority order: JSON > Apache/Nginx > Python > Go logrus > Log4j > Unknown
///
/// # Arguments
/// * `sample_lines` - Sample lines to analyze (first 100 lines recommended)
///
/// # Returns
/// The detected log format
pub fn detect_format(sample_lines: &[&str]) -> LogFormat {
    if sample_lines.is_empty() {
        return LogFormat::Unknown;
    }

    // Count JSON lines (check if line starts with '{')
    let json_count = sample_lines
        .iter()
        .filter(|line| line.trim_start().starts_with('{'))
        .count();

    // If >70% look like JSON, it's JSON
    if json_count as f64 / sample_lines.len() as f64 > 0.7 {
        return LogFormat::Json;
    }

    // Check common text patterns
    for line in sample_lines {
        // logfmt: level=info msg="..." (checked via the real pair parser)
        if crate::parser::parsers::logfmt::parse_pairs(line).is_some() {
            return LogFormat::Logfmt;
        }

        // Apache/Nginx: IP - - [timestamp] "method" status size
        if APACHE_PATTERN.is_match(line) {
            return LogFormat::ApacheCombined;
        }

        // Python: 2024-03-25 10:00:00,000 - INFO - module - message
        // or the docs-canonical order: ... - module - INFO - message
        if PYTHON_PATTERN.is_match(line) || PYTHON_NAME_FIRST_PATTERN.is_match(line) {
            return LogFormat::PythonLogging;
        }

        // Go logrus: 2024/03/25 10:00:00 INFO file.go:123: message
        if GO_PATTERN.is_match(line) {
            return LogFormat::GoLogrus;
        }

        // Log4j: 2024-03-25 10:00:00,000 INFO  [thread] Logger - message
        if LOG4J_PATTERN.is_match(line) {
            return LogFormat::Log4j;
        }

        // Rails: [<request id>] Started GET "/" for ... — the tag alone is
        // too loose to match on, so detection requires one of the three
        // lines Rails actually generates. Other tagged lines in the file are
        // still parsed once the file is known to be Rails.
        if RAILS_PATTERN.is_match(line) {
            return LogFormat::Rails;
        }

        // Syslog last: RFC3164's shape is loose, so specific formats win
        if SYSLOG_5424_PATTERN.is_match(line) || SYSLOG_3164_PATTERN.is_match(line) {
            return LogFormat::Syslog;
        }
    }

    LogFormat::Unknown
}

/// Pre-compiled regex patterns for speed
/// Using lazy_static for compile-time regex compilation
/// Apache/Nginx Combined Log Format
/// Pattern: 127.0.0.1 - - [25/Mar/2024:10:00:00 +0000] "GET /path HTTP/1.1" 200 1234
static APACHE_PATTERN: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r#"^\S+\s+\S+\s+\S+\s+\[.+?\]\s+"\S+\s+\S+\s+HTTP/\d\.\d"\s+\d+\s+\d+"#).unwrap()
});

/// Python logging format
/// Pattern: 2024-03-25 10:00:00,000 - INFO - module:name - message
/// Or: 2024-03-25 10:00:00,123 | INFO | module | message
static PYTHON_PATTERN: Lazy<Regex> = Lazy::new(|| {
    Regex::new(
        r"^\d{4}-\d{2}-\d{2}\s+\d{2}:\d{2}:\d{2}[,.]\d{3}\s+[-|]\s+(DEBUG|INFO|WARN|WARNING|ERROR|CRITICAL)",
    )
    .unwrap()
});

/// Python logging with the docs-canonical field order
/// (`%(asctime)s - %(name)s - %(levelname)s - %(message)s`):
/// Pattern: 2024-03-25 10:00:00,000 - app.auth - ERROR - message
static PYTHON_NAME_FIRST_PATTERN: Lazy<Regex> = Lazy::new(|| {
    Regex::new(
        r"^\d{4}-\d{2}-\d{2}\s+\d{2}:\d{2}:\d{2}[,.]\d{3}\s+[-|]\s+[\w.:]+\s+[-|]\s+(DEBUG|INFO|WARN|WARNING|ERROR|CRITICAL|FATAL)\s+[-|]",
    )
    .unwrap()
});

/// Go logrus format (text mode, not JSON)
/// Pattern: 2024/03/25 10:00:00 INFO file.go:123: message
static GO_PATTERN: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"^\d{4}/\d{2}/\d{2}\s+\d{2}:\d{2}:\d{2}\s+(DEBUG|INFO|WARN|ERROR|FATAL|PANIC)")
        .unwrap()
});

/// Rails / ActiveSupport::TaggedLogging.
/// Pattern: [<request id>] Started|Processing by|Completed ...
///
/// Detection is per line, so this has to recognise every line of a request,
/// not just the three Rails generates — otherwise the bulk of a Rails log
/// (queries, warnings, `Cannot render console from ...`) falls through to raw
/// and loses the request id that ties a request together.
///
/// Two ways to qualify. A UUID tag is claimed on sight: that is Rails' default
/// request id and nothing else writes it in leading brackets. Any other tag
/// must be followed by a line Rails itself emits, because `[anything] message`
/// is far too common a shape to take on the tag alone.
static RAILS_PATTERN: Lazy<Regex> = Lazy::new(|| {
    Regex::new(concat!(
        r"^\[[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}\]\s+\S",
        r#"|^\[[A-Za-z0-9][A-Za-z0-9._:-]{7,}\]\s+(?:Started\s+[A-Z]+\s+"|Processing by\s+\S+|Completed\s+\d{3}\b)"#,
    ))
    .unwrap()
});

/// Log4j/Log4j2 pattern
/// Pattern: 2024-03-25 10:00:00,000 INFO  [thread] LoggerName - message
///
/// The fractional second is optional, matching the parser: `%d{yyyy-MM-dd
/// HH:mm:ss}` is a common layout. The level keyword immediately after the
/// timestamp keeps this from claiming Python's `<ts> - <logger> - <LEVEL>`.
static LOG4J_PATTERN: Lazy<Regex> = Lazy::new(|| {
    Regex::new(
        r"^\d{4}-\d{2}-\d{2}\s+\d{2}:\d{2}:\d{2}(?:[,.]\d{1,9})?\s+(TRACE|DEBUG|INFO|WARN|ERROR|FATAL)",
    )
    .unwrap()
});

/// Syslog RFC5424: <165>1 2026-07-15T10:00:00Z host app ...
static SYSLOG_5424_PATTERN: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"^<\d{1,3}>\d\s+\S+\s+\S+\s+\S+").unwrap());

/// Syslog RFC3164: Jul 15 09:10:00 host proc[pid]: message
static SYSLOG_3164_PATTERN: Lazy<Regex> = Lazy::new(|| {
    Regex::new(
        r"^(?:<\d{1,3}>)?(Jan|Feb|Mar|Apr|May|Jun|Jul|Aug|Sep|Oct|Nov|Dec)\s+\d{1,2}\s+\d{2}:\d{2}:\d{2}\s+\S+\s+[\w\-./]+(\[\d+\])?:",
    )
    .unwrap()
});

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_detect_json_format() {
        let json_lines = vec![
            r#"{"timestamp":"2024-03-25T10:00:00Z","level":"info","message":"test"}"#,
            r#"{"timestamp":"2024-03-25T10:00:01Z","level":"info","message":"test2"}"#,
            r#"{"timestamp":"2024-03-25T10:00:02Z","level":"error","message":"error"}"#,
        ];

        assert_eq!(detect_format(&json_lines), LogFormat::Json);
    }

    #[test]
    fn test_detect_apache_format() {
        let apache_lines = vec![
            r#"127.0.0.1 - - [25/Mar/2024:10:00:00 +0000] "GET /api/users HTTP/1.1" 200 1234"#,
            r#"192.168.1.1 - - [25/Mar/2024:10:00:01 +0000] "POST /api/auth HTTP/1.1" 401 567"#,
        ];

        assert_eq!(detect_format(&apache_lines), LogFormat::ApacheCombined);
    }

    #[test]
    fn test_detect_python_format() {
        let python_lines = vec![
            r#"2024-03-25 10:00:00,000 - INFO - app - Starting server"#,
            r#"2024-03-25 10:00:01,123 - ERROR - database - Connection failed"#,
        ];

        assert_eq!(detect_format(&python_lines), LogFormat::PythonLogging);
    }

    #[test]
    fn test_detect_go_logrus_format() {
        let go_lines = vec![
            r#"2024/03/25 10:00:00 INFO main.go:45: Starting server"#,
            r#"2024/03/25 10:00:01 ERROR db.go:123: Connection failed"#,
        ];

        assert_eq!(detect_format(&go_lines), LogFormat::GoLogrus);
    }

    #[test]
    fn test_detect_log4j_format() {
        let log4j_lines = vec![
            r#"2024-03-25 10:00:00,000 INFO  [main] com.example.App - Starting"#,
            r#"2024-03-25 10:00:01,123 ERROR [http-nio-8080] com.example.DB - Error"#,
        ];

        assert_eq!(detect_format(&log4j_lines), LogFormat::Log4j);
    }

    #[test]
    fn test_detect_unknown_format() {
        let unknown_lines = vec![
            r#"Some random log line"#,
            r#"Another random line without pattern"#,
        ];

        assert_eq!(detect_format(&unknown_lines), LogFormat::Unknown);
    }

    #[test]
    fn test_detect_json_mixed_with_noise() {
        // 70% JSON should still be detected as JSON
        let mixed_lines = vec![
            r#"{"timestamp":"2024-03-25T10:00:00Z","level":"info","message":"test"}"#,
            r#"{"timestamp":"2024-03-25T10:00:01Z","level":"info","message":"test2"}"#,
            r#"{"timestamp":"2024-03-25T10:00:02Z","level":"error","message":"error"}"#,
            r#"random noise line"#,
        ];

        assert_eq!(detect_format(&mixed_lines), LogFormat::Json);
    }

    #[test]
    fn test_detect_empty_sample() {
        assert_eq!(detect_format(&[]), LogFormat::Unknown);
    }
}
