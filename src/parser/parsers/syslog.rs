//! Syslog parser: RFC5424 (`<pri>1 timestamp host app ...`) and the
//! traditional RFC3164 format (`Jul 15 09:10:00 host proc[pid]: message`).

use crate::parser::{log_format::LogFormat, LogParser};
use crate::schema::RawLogEntry;
use once_cell::sync::Lazy;
use regex::Regex;

pub struct SyslogParser;

/// RFC5424: <165>1 2026-07-15T10:00:00.000Z host app 1234 MSGID [sd] message
#[allow(clippy::unwrap_used)] // compile-time-constant pattern
static RFC5424: Lazy<Regex> = Lazy::new(|| {
    Regex::new(
        r"^<(?P<pri>\d{1,3})>(?P<version>\d)\s+(?P<ts>\S+)\s+(?P<host>\S+)\s+(?P<app>\S+)\s+(?P<procid>\S+)\s+(?P<msgid>\S+)\s+(?:-|\[[^\]]*\])\s*(?P<msg>.*)$",
    )
    .unwrap()
});

/// RFC3164: Jul 15 09:10:00 myhost sshd[4123]: Failed password for root
#[allow(clippy::unwrap_used)]
static RFC3164: Lazy<Regex> = Lazy::new(|| {
    Regex::new(
        r"^(?:<(?P<pri>\d{1,3})>)?(?P<month>Jan|Feb|Mar|Apr|May|Jun|Jul|Aug|Sep|Oct|Nov|Dec)\s+(?P<day>\d{1,2})\s+(?P<time>\d{2}:\d{2}:\d{2})\s+(?P<host>\S+)\s+(?P<proc>[\w\-./]+)(?:\[(?P<pid>\d+)\])?:\s*(?P<msg>.*)$",
    )
    .unwrap()
});

impl LogParser for SyslogParser {
    fn parse(&self, line: &str, source: &str) -> Option<RawLogEntry> {
        if let Some(caps) = RFC5424.captures(line) {
            let pri: u8 = caps.name("pri")?.as_str().parse().ok()?;
            let ts = caps.name("ts")?.as_str();
            let timestamp = if ts == "-" {
                None
            } else {
                Some(ts.to_string())
            };
            return Some(RawLogEntry {
                timestamp,
                level: Some(severity_to_level(pri % 8)),
                message: Some(caps.name("msg")?.as_str().to_string()),
                source: Some(source.to_string()),
                source_file: caps.name("app").map(|m| m.as_str().to_string()),
                ..Default::default()
            });
        }

        if let Some(caps) = RFC3164.captures(line) {
            let level = caps
                .name("pri")
                .and_then(|p| p.as_str().parse::<u8>().ok())
                .map(|pri| severity_to_level(pri % 8));
            let timestamp = rfc3164_timestamp(
                caps.name("month")?.as_str(),
                caps.name("day")?.as_str(),
                caps.name("time")?.as_str(),
            );
            return Some(RawLogEntry {
                timestamp,
                // RFC3164 without a <pri> prefix carries no severity.
                level,
                message: Some(caps.name("msg")?.as_str().to_string()),
                source: Some(source.to_string()),
                source_file: caps.name("proc").map(|m| m.as_str().to_string()),
                ..Default::default()
            });
        }

        None
    }

    fn format_name(&self) -> &str {
        "syslog"
    }

    fn format(&self) -> LogFormat {
        LogFormat::Syslog
    }
}

/// Map syslog severity (0-7) to our level vocabulary.
fn severity_to_level(severity: u8) -> String {
    match severity {
        0..=3 => "error",
        4 => "warn",
        5 | 6 => "info",
        _ => "debug",
    }
    .to_string()
}

/// RFC3164 timestamps carry no year: assume the current year (correct for
/// live dev logs, which is our use case).
fn rfc3164_timestamp(month: &str, day: &str, time: &str) -> Option<String> {
    let month_num = match month {
        "Jan" => 1,
        "Feb" => 2,
        "Mar" => 3,
        "Apr" => 4,
        "May" => 5,
        "Jun" => 6,
        "Jul" => 7,
        "Aug" => 8,
        "Sep" => 9,
        "Oct" => 10,
        "Nov" => 11,
        "Dec" => 12,
        _ => return None,
    };
    let year = chrono::Utc::now().format("%Y");
    Some(format!(
        "{}-{:02}-{:02}T{}.000Z",
        year,
        month_num,
        day.parse::<u8>().ok()?,
        time
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_rfc3164() {
        let line = "Jul 15 09:10:00 myhost sshd[4123]: Failed password for root from 10.0.0.1";
        let raw = SyslogParser.parse(line, "syslog").unwrap();
        assert_eq!(
            raw.message,
            Some("Failed password for root from 10.0.0.1".to_string())
        );
        assert_eq!(raw.source_file, Some("sshd".to_string()));
        assert!(raw.timestamp.unwrap().contains("-07-15T09:10:00"));
        // No <pri> prefix: severity unknown, defaulted downstream to info.
        assert_eq!(raw.level, None);
    }

    #[test]
    fn test_parse_rfc3164_with_pri() {
        let line = "<11>Jul 15 09:10:00 myhost app: disk failure imminent";
        let raw = SyslogParser.parse(line, "syslog").unwrap();
        // pri 11 = facility 1, severity 3 (err)
        assert_eq!(raw.level, Some("error".to_string()));
    }

    #[test]
    fn test_parse_rfc5424() {
        let line = r#"<165>1 2026-07-15T10:00:00.000Z web01 checkout 8710 ID47 - payment service degraded"#;
        let raw = SyslogParser.parse(line, "syslog").unwrap();
        // pri 165 = facility 20, severity 5 (notice) → info
        assert_eq!(raw.level, Some("info".to_string()));
        assert_eq!(raw.message, Some("payment service degraded".to_string()));
        assert_eq!(raw.timestamp, Some("2026-07-15T10:00:00.000Z".to_string()));
        assert_eq!(raw.source_file, Some("checkout".to_string()));
    }

    #[test]
    fn test_non_syslog_rejected() {
        assert!(SyslogParser.parse("just some text", "syslog").is_none());
        assert!(SyslogParser
            .parse(r#"{"level":"info","message":"json"}"#, "syslog")
            .is_none());
    }
}
