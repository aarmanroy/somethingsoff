//! logfmt parser: `key=value key2="quoted value"` pairs (the Go/Heroku
//! ecosystem standard).
//!
//! Pairs are collected into a JSON object and mapped through
//! `RawLogEntry::from_value`, so all the usual aliases (ts/time/msg/level)
//! apply and unrecognized keys land in `attributes`.

use crate::parser::{log_format::LogFormat, LogParser};
use crate::schema::RawLogEntry;

pub struct LogfmtParser;

impl LogParser for LogfmtParser {
    fn parse(&self, line: &str, source: &str) -> Option<RawLogEntry> {
        let pairs = parse_pairs(line)?;
        let mut map = serde_json::Map::new();
        for (key, value) in pairs {
            map.insert(key, serde_json::Value::String(value));
        }
        let mut raw = RawLogEntry::from_value(serde_json::Value::Object(map))?;
        if raw.source.is_none() {
            raw.source = Some(source.to_string());
        }
        Some(raw)
    }

    fn format_name(&self) -> &str {
        "logfmt"
    }

    fn format(&self) -> LogFormat {
        LogFormat::Logfmt
    }
}

/// Split a logfmt line into key=value pairs, honoring double quotes with
/// backslash escapes. Returns None if the line doesn't look like logfmt
/// (needs at least 2 pairs, one of which is a known log key).
pub fn parse_pairs(line: &str) -> Option<Vec<(String, String)>> {
    let mut pairs = Vec::new();
    let mut chars = line.trim().chars().peekable();

    while chars.peek().is_some() {
        // Skip whitespace between pairs
        while chars.peek().is_some_and(|c| c.is_whitespace()) {
            chars.next();
        }
        if chars.peek().is_none() {
            break;
        }

        // Key: up to '='
        let mut key = String::new();
        for c in chars.by_ref() {
            if c == '=' {
                break;
            }
            if c.is_whitespace() {
                // A bare word without '=' — not a pair; not strict logfmt.
                return None;
            }
            key.push(c);
        }
        if key.is_empty() {
            return None;
        }

        // Value: quoted (with escapes) or bare
        let mut value = String::new();
        if chars.peek() == Some(&'"') {
            chars.next(); // consume opening quote
            let mut escaped = false;
            for c in chars.by_ref() {
                if escaped {
                    value.push(c);
                    escaped = false;
                } else if c == '\\' {
                    escaped = true;
                } else if c == '"' {
                    break;
                } else {
                    value.push(c);
                }
            }
        } else {
            while let Some(&c) = chars.peek() {
                if c.is_whitespace() {
                    break;
                }
                value.push(c);
                chars.next();
            }
        }

        pairs.push((key, value));
    }

    // Heuristic gate: at least 2 pairs and at least one recognizable log key
    // so ordinary prose containing one '=' isn't misparsed as logfmt.
    const KNOWN_KEYS: [&str; 8] = [
        "level",
        "lvl",
        "msg",
        "message",
        "time",
        "ts",
        "timestamp",
        "severity",
    ];
    if pairs.len() >= 2 && pairs.iter().any(|(k, _)| KNOWN_KEYS.contains(&k.as_str())) {
        Some(pairs)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_basic_logfmt() {
        let line = r#"time=2026-07-15T10:00:00Z level=error msg="db connection refused" service=checkout retries=3"#;
        let raw = LogfmtParser.parse(line, "app").unwrap();
        assert_eq!(raw.level, Some("error".to_string()));
        assert_eq!(raw.message, Some("db connection refused".to_string()));
        assert_eq!(raw.timestamp, Some("2026-07-15T10:00:00Z".to_string()));
        // Unrecognized keys preserved as attributes
        assert_eq!(
            raw.extra.get("service"),
            Some(&serde_json::Value::String("checkout".to_string()))
        );
        assert_eq!(
            raw.extra.get("retries"),
            Some(&serde_json::Value::String("3".to_string()))
        );
    }

    #[test]
    fn test_parse_logrus_style() {
        let line = r#"time="2026-07-15 10:00:00" level=warning msg="cache miss rate high""#;
        let raw = LogfmtParser.parse(line, "app").unwrap();
        assert_eq!(raw.message, Some("cache miss rate high".to_string()));
    }

    #[test]
    fn test_escaped_quotes_in_value() {
        let line = r#"level=info msg="user said \"hello\" today""#;
        let raw = LogfmtParser.parse(line, "app").unwrap();
        assert_eq!(raw.message, Some(r#"user said "hello" today"#.to_string()));
    }

    #[test]
    fn test_prose_with_equals_is_rejected() {
        assert!(LogfmtParser
            .parse("the answer = 42 obviously", "app")
            .is_none());
        assert!(LogfmtParser.parse("x=1", "app").is_none()); // single pair, no known key
    }
}
