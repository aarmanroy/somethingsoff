//! Apache/Nginx Combined Log Format parser
//!
//! Parses Apache and Nginx access logs in Combined Log Format:
//! - Format: 127.0.0.1 - - [25/Mar/2024:10:00:00 +0000] "GET /path HTTP/1.1" 200 1234 "http://referer" "Mozilla/5.0"

use crate::parser::{log_format::LogFormat, LogParser};
use crate::schema::RawLogEntry;
use once_cell::sync::Lazy;
use regex::Regex;

/// Apache/Nginx Combined Log Format parser
pub struct ApacheParser;

/// Apache/Nginx Combined Log Format (full with referer and user-agent)
/// Format: 127.0.0.1 - - [25/Mar/2024:10:00:00 +0000] "GET /path HTTP/1.1" 200 1234 "http://referer" "Mozilla/5.0"
static APACHE_FULL_PATTERN: Lazy<Regex> = Lazy::new(|| {
    Regex::new(
        r#"^(?P<ip>[\d.:a-fA-F]+)\s+-\s+-\s+\[(?P<timestamp>\d{2}/[A-Za-z]{3}/\d{4}:\d{2}:\d{2}:\d{2}\s+[+-]\d{4})\]\s+"(?P<method>[A-Z]+)\s+(?P<path>[^"]+)\s+HTTP/\d+\.\d+"\s+(?P<status>\d{3})\s+(?P<size>\d+)(?:\s+"(?P<referer>[^"]*)"\s+"(?P<user_agent>[^"]*)")?"#
    )
    .unwrap()
});

/// Apache/Nginx Common Log Format (without referer and user-agent)
static APACHE_COMMON_PATTERN: Lazy<Regex> = Lazy::new(|| {
    Regex::new(
        r#"^(?P<ip>[\d.:a-fA-F]+)\s+-\s+-\s+\[(?P<timestamp>\d{2}/[A-Za-z]{3}/\d{4}:\d{2}:\d{2}:\d{2}\s+[+-]\d{4})\]\s+"(?P<method>[A-Z]+)\s+(?P<path>[^"]+)\s+HTTP/\d+\.\d+"\s+(?P<status>\d{3})\s+(?P<size>\d+)"#
    )
    .unwrap()
});

impl LogParser for ApacheParser {
    fn parse(&self, line: &str, source: &str) -> Option<RawLogEntry> {
        // Try full pattern first (with referer and user-agent)
        if let Some(caps) = APACHE_FULL_PATTERN.captures(line) {
            let status_code: u16 = caps.name("status")?.as_str().parse().ok()?;
            let _size_bytes: u64 = caps.name("size")?.as_str().parse().ok()?;

            let level = if status_code >= 500 {
                "error"
            } else if status_code >= 400 {
                "warn"
            } else {
                "info"
            };

            // user_agent is optional in the regex
            let user_agent = caps.name("user_agent").map(|m| m.as_str()).unwrap_or("-");

            let message = format!(
                "{} {} - {}",
                caps.name("method")?.as_str(),
                caps.name("path")?.as_str(),
                user_agent
            );

            return Some(RawLogEntry {
                timestamp: Some(parse_apache_timestamp(caps.name("timestamp")?.as_str())?),
                level: Some(level.to_string()),
                message: Some(message),
                source: Some(source.to_string()),
                method: Some(caps.name("method")?.as_str().to_string()),
                route: Some(caps.name("path")?.as_str().to_string()),
                status_code: Some(status_code),
                ..Default::default()
            });
        }

        // Try common pattern (without referer and user-agent)
        if let Some(caps) = APACHE_COMMON_PATTERN.captures(line) {
            let status_code: u16 = caps.name("status")?.as_str().parse().ok()?;
            let _size_bytes: u64 = caps.name("size")?.as_str().parse().ok()?;

            let level = if status_code >= 500 {
                "error"
            } else if status_code >= 400 {
                "warn"
            } else {
                "info"
            };

            let message = format!(
                "{} {}",
                caps.name("method")?.as_str(),
                caps.name("path")?.as_str()
            );

            return Some(RawLogEntry {
                timestamp: Some(parse_apache_timestamp(caps.name("timestamp")?.as_str())?),
                level: Some(level.to_string()),
                message: Some(message),
                source: Some(source.to_string()),
                method: Some(caps.name("method")?.as_str().to_string()),
                route: Some(caps.name("path")?.as_str().to_string()),
                status_code: Some(status_code),
                ..Default::default()
            });
        }

        None
    }

    fn format_name(&self) -> &str {
        "apache-combined"
    }

    fn format(&self) -> LogFormat {
        LogFormat::ApacheCombined
    }
}

/// Parse Apache timestamp to ISO8601 format
/// Input: 25/Mar/2024:10:00:00 +0000
/// Output: 2024-03-25T10:00:00.000Z
fn parse_apache_timestamp(ts: &str) -> Option<String> {
    // Apache format: 25/Mar/2024:10:00:00 +0000
    // Need to parse day/month/year:hour:minute:second timezone

    let parts: Vec<&str> = ts.split_whitespace().collect::<Vec<_>>();
    if parts.len() < 2 {
        return None;
    }

    let datetime_part = parts[0]; // 25/Mar/2024:10:00:00
    let _tz_part = parts[1]; // +0000 (ignored, assume UTC)

    let datetime_parts: Vec<&str> = datetime_part.split(':').collect::<Vec<_>>();
    if datetime_parts.len() != 4 {
        return None;
    }

    let date_part = datetime_parts[0]; // 25/Mar/2024
    let hour = datetime_parts[1]; // 10
    let minute = datetime_parts[2]; // 00
    let second = datetime_parts[3]; // 00

    // Split date part
    let date_parts: Vec<&str> = date_part.split('/').collect::<Vec<_>>();
    if date_parts.len() != 3 {
        return None;
    }

    let day = date_parts[0]; // 25
    let month_str = date_parts[1]; // Mar
    let year = date_parts[2]; // 2024

    // Convert month abbreviation to number
    let month_num = match month_str.to_lowercase().as_str() {
        "jan" => "01",
        "feb" => "02",
        "mar" => "03",
        "apr" => "04",
        "may" => "05",
        "jun" => "06",
        "jul" => "07",
        "aug" => "08",
        "sep" => "09",
        "oct" => "10",
        "nov" => "11",
        "dec" => "12",
        _ => return None,
    };

    Some(format!(
        "{}-{}-{}T{}:{}:{}.000Z",
        year, month_num, day, hour, minute, second
    ))
}

#[cfg(test)]
mod tests {
    #[test]
    fn test_debug_regex() {
        let line = r#"127.0.0.1 - - [25/Mar/2024:10:00:00 +0000] "GET / HTTP/1.1" 200 1234"#;
        println!("Testing line: {}", line);
        println!("Pattern: {}", APACHE_COMMON_PATTERN.as_str());
        println!("Is match: {}", APACHE_COMMON_PATTERN.is_match(line));

        if let Some(caps) = APACHE_COMMON_PATTERN.captures(line) {
            println!("Match found!");
            for name in APACHE_COMMON_PATTERN.capture_names().flatten() {
                println!("  {}: {:?}", name, caps.name(name));
            }
        } else {
            println!("No match!");
        }
    }

    #[test]
    fn test_debug_full_pattern() {
        let line = r#"127.0.0.1 - - [25/Mar/2024:10:00:00 +0000] "GET /api/users HTTP/1.1" 200 1234 "http://example.com" "Mozilla/5.0""#;
        println!("Testing full line: {}", line);
        println!("Full Pattern: {}", APACHE_FULL_PATTERN.as_str());
        println!("Is match: {}", APACHE_FULL_PATTERN.is_match(line));

        if let Some(caps) = APACHE_FULL_PATTERN.captures(line) {
            println!("Match found!");
            for name in APACHE_FULL_PATTERN.capture_names().flatten() {
                println!("  {}: {:?}", name, caps.name(name));
            }
        } else {
            println!("No match!");
        }
    }
    use super::*;

    #[test]
    fn test_parse_apache_combined_full() {
        let line = r#"127.0.0.1 - - [25/Mar/2024:10:00:00 +0000] "GET /api/users HTTP/1.1" 200 1234 "http://example.com" "Mozilla/5.0""#;
        let parser = ApacheParser;
        let result = parser.parse(line, "nginx");

        assert!(result.is_some());
        let entry = result.unwrap();
        assert_eq!(entry.method, Some("GET".to_string()));
        assert_eq!(entry.route, Some("/api/users".to_string()));
        assert_eq!(entry.status_code, Some(200));
        assert_eq!(entry.level, Some("info".to_string()));
    }

    #[test]
    fn test_parse_apache_error_status() {
        let line = r#"127.0.0.1 - - [25/Mar/2024:10:00:00 +0000] "POST /api/auth HTTP/1.1" 500 567 "-" "curl/7.68.0""#;
        let parser = ApacheParser;
        let result = parser.parse(line, "apache");

        assert!(result.is_some());
        let entry = result.unwrap();
        assert_eq!(entry.status_code, Some(500));
        assert_eq!(entry.level, Some("error".to_string()));
    }

    #[test]
    fn test_parse_apache_4xx_status() {
        let line = r#"127.0.0.1 - - [25/Mar/2024:10:00:00 +0000] "GET /missing HTTP/1.1" 404 123 "-" "curl/7.68.0""#;
        let parser = ApacheParser;
        let result = parser.parse(line, "nginx");

        assert!(result.is_some());
        let entry = result.unwrap();
        assert_eq!(entry.status_code, Some(404));
        assert_eq!(entry.level, Some("warn".to_string()));
    }

    #[test]
    fn test_parse_apache_combined_minimal() {
        let line = r#"127.0.0.1 - - [25/Mar/2024:10:00:00 +0000] "GET / HTTP/1.1" 200 1234"#;
        let parser = ApacheParser;
        let result = parser.parse(line, "apache");

        assert!(result.is_some());
        let entry = result.unwrap();
        assert_eq!(entry.method, Some("GET".to_string()));
        assert_eq!(entry.route, Some("/".to_string()));
    }

    #[test]
    fn test_parse_apache_ipv6() {
        let line = r#"2001:db8::1 - - [25/Mar/2024:10:00:00 +0000] "GET /api HTTP/1.1" 200 1234"#;
        let parser = ApacheParser;
        let result = parser.parse(line, "nginx");

        assert!(result.is_some());
    }

    #[test]
    fn test_parse_non_apache_returns_none() {
        let line = r#"{"json": "format"}"#;
        let parser = ApacheParser;
        let result = parser.parse(line, "apache");

        assert!(result.is_none());
    }

    #[test]
    fn test_format_name() {
        assert_eq!(ApacheParser.format_name(), "apache-combined");
    }

    #[test]
    fn test_format() {
        assert_eq!(ApacheParser.format(), LogFormat::ApacheCombined);
    }
}
