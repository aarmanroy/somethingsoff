//! Log format parsers
//!
//! Individual parsers for each supported log format

pub mod apache;
pub mod go_logrus;
pub mod log4j;
pub mod logfmt;
pub mod python;
pub mod syslog;

use crate::parser::detector::detect_format;
use crate::parser::log_format::LogFormat;
use crate::schema::RawLogEntry;
use once_cell::sync::Lazy;
use regex::Regex;

/// Trait for log format parsers
pub trait LogParser: Send + Sync {
    /// Parse a single line into a RawLogEntry
    ///
    /// Returns None if the line doesn't match this parser's format
    fn parse(&self, line: &str, source: &str) -> Option<RawLogEntry>;

    /// Get the format name for this parser
    fn format_name(&self) -> &str;

    /// Get the log format enum value
    fn format(&self) -> LogFormat;
}

/// Parse a log entry with automatic format detection.
///
/// Tries each structured parser in priority order (JSON, logfmt, Apache,
/// Python, Go logrus, Log4j, syslog). If none claims the line, the line is
/// **not dropped**: it is captured as a raw text entry (`message` = the line,
/// with ANSI stripped and a best-effort level sniffed). This makes ingest
/// lossless, so `tap`/auto-sync work on any output — plain dev-server logs,
/// build output, test runners — with zero instrumentation.
///
/// # Arguments
/// * `line` - The log line to parse
/// * `source` - Default source name if not in the log
///
/// # Returns
/// `Some(RawLogEntry)` for any line with real content; `None` only for empty,
/// whitespace, or pure-decoration lines (ANSI escapes, box-drawing rules) that
/// carry nothing to index.
pub fn parse_log_entry(line: &str, source: &str) -> Option<RawLogEntry> {
    let line = line.trim();

    // Skip empty lines
    if line.is_empty() {
        return None;
    }

    // Detect format from single line
    let format = detect_format(&[line]);

    // Use appropriate parser
    let structured = match format {
        LogFormat::Json => crate::parser::json::JsonParser.parse(line, source),
        LogFormat::Logfmt => logfmt::LogfmtParser.parse(line, source),
        LogFormat::ApacheCombined => apache::ApacheParser.parse(line, source),
        LogFormat::PythonLogging => python::PythonParser.parse(line, source),
        LogFormat::GoLogrus => go_logrus::GoLogrusParser.parse(line, source),
        LogFormat::Log4j => log4j::Log4jParser.parse(line, source),
        LogFormat::Syslog => syslog::SyslogParser.parse(line, source),
        LogFormat::Unknown => None,
    };

    // Lossless fallback: a line no structured parser matched (or that matched
    // detection but failed to parse) is still real content — capture it raw.
    // Either way, stamp the entry with its parse origin so structured-vs-raw
    // share stays queryable (`stats --by-format`, `search --parse-format`).
    match structured {
        Some(mut entry) => {
            entry.parse_format = Some(format.name().to_string());
            Some(entry)
        }
        None => fallback_entry(line, source).map(|mut entry| {
            entry.parse_format = Some("raw".to_string());
            entry
        }),
    }
}

/// Parse one coalesced block (see `parser::coalesce`).
///
/// Single-line blocks take exactly the [`parse_log_entry`] path. For
/// multiline blocks: if the first line parses structured (e.g. a log4j line
/// whose stack trace got glued), the continuation is appended to its
/// message; otherwise the whole block becomes ONE raw entry — message = the
/// full ANSI-stripped block, level sniffed across all of it (so an
/// `error:` anchor marks the entire diagnostic as an error).
pub fn parse_block(text: &str, source: &str) -> Option<RawLogEntry> {
    let Some((first, rest)) = text.split_once('\n') else {
        return parse_log_entry(text, source);
    };

    match parse_log_entry(first, source) {
        Some(mut entry) if entry.parse_format.as_deref() != Some("raw") => {
            let continuation = strip_ansi(rest);
            let continuation = continuation.trim_end();
            if !continuation.is_empty() {
                entry.message = Some(match entry.message.take() {
                    Some(m) => format!("{}\n{}", m, continuation),
                    None => continuation.to_string(),
                });
            }
            Some(entry)
        }
        _ => {
            let cleaned = strip_ansi(text);
            let trimmed = cleaned.trim();
            if !trimmed.chars().any(|c| c.is_alphanumeric()) {
                return None;
            }
            Some(RawLogEntry {
                message: Some(trimmed.to_string()),
                level: sniff_level(trimmed),
                parse_format: Some("raw".to_string()),
                ..Default::default()
            })
        }
    }
}

/// Matches ANSI/VT100 escape sequences: CSI (colors, cursor moves, erase)
/// and OSC (terminated by BEL or ST) — cargo emits OSC 8 hyperlinks around
/// "Finished/Compiling" lines, which would otherwise index as garbage.
static ANSI_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new("\x1b\\[[0-9;?]*[ -/]*[@-~]|\x1b\\][^\x07\x1b]*(\x07|\x1b\\\\)").unwrap()
});

/// Word-boundary tokens that imply an error/warn/debug level in free text.
static ERROR_RE: Lazy<Regex> = Lazy::new(|| {
    // `\w*errors?` (not `\b`-anchored `error`) so exception-class names match:
    // "TypeError:", "DatabaseException", Rust's "panicked at" — word-boundary
    // regexes can't see a token embedded mid-word. Failure prose without an
    // error token ("Cannot find package X", "ESLint couldn't find a config")
    // and Node/OS error codes (ENOENT, ERR_MODULE_NOT_FOUND) count too: the
    // whatbroke metric showed such failures sniffing as info and vanishing
    // from `errors`. E[A-Z_]{3,} does not match e.g. "ESLint" (lowercase
    // follows without a word boundary).
    // NB: (?i:...) is scoped — the E[A-Z_]{3,} code-pattern must stay
    // case-sensitive or it would match any 4+ letter word starting with 'e'.
    Regex::new(
        r"(?i:\b(\w*errors?|\w*exceptions?|fatal|panic(ked|king)?|traceback|fail|failed|failing|failure|cannot|can't|could ?not|couldn't|unable to|not found|no such)\b)|\bE[A-Z_]{3,}\b",
    )
    .unwrap()
});
static WARN_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?i)\b(warn|warning|deprecated|deprecation)\b").unwrap());
static DEBUG_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?i)\b(debug|trace)\b").unwrap());

/// Strip ANSI escape sequences so indexed text is searchable and clean.
pub fn strip_ansi(line: &str) -> String {
    ANSI_RE.replace_all(line, "").into_owned()
}

/// Best-effort log level for an unstructured line. `None` → caller's default
/// (`info`). Priority: error > warn > debug.
fn sniff_level(text: &str) -> Option<String> {
    if ERROR_RE.is_match(text) || text.contains('✗') || text.contains('×') || text.contains('✘')
    {
        Some("error".to_string())
    } else if WARN_RE.is_match(text) {
        Some("warn".to_string())
    } else if DEBUG_RE.is_match(text) {
        Some("debug".to_string())
    } else {
        None
    }
}

/// Turn an unrecognized line into a raw text entry. Returns `None` for lines
/// with no alphanumeric content (blank, ANSI-only, box-drawing decoration,
/// non-UTF-8 replacement chars) — there is nothing worth indexing.
fn fallback_entry(line: &str, _source: &str) -> Option<RawLogEntry> {
    let cleaned = strip_ansi(line);
    let trimmed = cleaned.trim();
    if !trimmed.chars().any(|c| c.is_alphanumeric()) {
        return None;
    }
    Some(RawLogEntry {
        message: Some(trimmed.to_string()),
        level: sniff_level(trimmed),
        ..Default::default()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_json_entry() {
        let line = r#"{"timestamp":"2024-03-25T10:00:00Z","level":"info","message":"test"}"#;
        let result = parse_log_entry(line, "test");

        assert!(result.is_some());
        let entry = result.unwrap();
        assert_eq!(entry.message, Some("test".to_string()));
        assert_eq!(entry.level, Some("info".to_string()));
    }

    #[test]
    fn test_parse_python_entry() {
        let line = r#"2024-03-25 10:00:00,000 - INFO - app - Starting server"#;
        let result = parse_log_entry(line, "python-app");

        assert!(result.is_some());
        let entry = result.unwrap();
        assert_eq!(entry.message, Some("Starting server".to_string()));
        assert_eq!(entry.level, Some("info".to_string()));
    }

    #[test]
    fn test_parse_go_logrus_entry() {
        let line = r#"2024/03/25 10:00:00 INFO main.go:45: Starting server"#;
        let result = parse_log_entry(line, "go-app");

        assert!(result.is_some());
        let entry = result.unwrap();
        assert_eq!(entry.message, Some("Starting server".to_string()));
        assert_eq!(entry.level, Some("info".to_string()));
    }

    #[test]
    fn test_parse_apache_entry() {
        let line =
            r#"127.0.0.1 - - [25/Mar/2024:10:00:00 +0000] "GET /api/users HTTP/1.1" 200 1234"#;
        let result = parse_log_entry(line, "nginx");

        assert!(result.is_some());
        let entry = result.unwrap();
        assert!(entry.message.is_some());
        assert_eq!(entry.method, Some("GET".to_string()));
        assert_eq!(entry.status_code, Some(200));
    }

    #[test]
    fn test_parse_log4j_entry() {
        let line = r#"2024-03-25 10:00:00,000 INFO  [main] com.example.App - Starting"#;
        let result = parse_log_entry(line, "java-app");

        assert!(result.is_some());
        let entry = result.unwrap();
        assert_eq!(entry.message, Some("Starting".to_string()));
        assert_eq!(entry.level, Some("info".to_string()));
    }

    #[test]
    fn test_parse_empty_line() {
        let result = parse_log_entry("", "test");
        assert!(result.is_none());
    }

    #[test]
    fn test_parse_whitespace_only() {
        let result = parse_log_entry("   ", "test");
        assert!(result.is_none());
    }

    #[test]
    fn test_parse_unknown_format_is_captured_raw() {
        // Unrecognized lines are no longer dropped: they become raw entries
        // so `tap`/auto-sync are lossless on any output.
        let line = "random text that doesn't match any format";
        let entry = parse_log_entry(line, "test").expect("unstructured line captured as raw");
        assert_eq!(entry.message.as_deref(), Some(line));
        assert_eq!(entry.level, None); // no level token → caller defaults to info
    }

    #[test]
    fn test_raw_fallback_strips_ansi_and_sniffs_level() {
        // Colored dev-server output: ANSI stripped, level sniffed from text.
        let entry =
            parse_log_entry("\x1b[31mERROR\x1b[0m  build failed", "web").expect("captured as raw");
        assert_eq!(entry.message.as_deref(), Some("ERROR  build failed"));
        assert_eq!(entry.level.as_deref(), Some("error"));
    }

    #[test]
    fn test_raw_fallback_sniffs_warn_and_debug() {
        assert_eq!(
            parse_log_entry("Warning: deprecated API in use", "x")
                .unwrap()
                .level
                .as_deref(),
            Some("warn")
        );
        assert_eq!(
            parse_log_entry("DEBUG connecting to pool", "x")
                .unwrap()
                .level
                .as_deref(),
            Some("debug")
        );
    }

    #[test]
    fn test_decoration_only_lines_are_skipped() {
        // Pure ANSI / box-drawing / rules carry nothing to index.
        assert!(parse_log_entry("\x1b[2K\x1b[1G", "x").is_none());
        assert!(parse_log_entry("├─────────────┤", "x").is_none());
        assert!(parse_log_entry("----------------", "x").is_none());
    }

    #[test]
    fn test_osc_hyperlink_escapes_are_stripped() {
        // Cargo wraps "Finished dev profile" in OSC 8 hyperlinks (found by the
        // format-torture sweep: the remnants indexed as `]8;;https://…\`).
        let line = "\x1b[1m\x1b[32mFinished\x1b[0m \x1b]8;;https://doc.rust-lang.org/cargo/reference/profiles.html#default-profiles\x1b\\`dev` profile\x1b]8;;\x1b\\ target(s) in 0.23s";
        let entry = parse_log_entry(line, "cargo").unwrap();
        assert_eq!(
            entry.message.as_deref(),
            Some("Finished `dev` profile target(s) in 0.23s")
        );
    }

    #[test]
    fn test_structured_entries_are_stamped_with_parser_name() {
        let json = r#"{"timestamp":"2024-03-25T10:00:00Z","level":"info","message":"m"}"#;
        assert_eq!(
            parse_log_entry(json, "t").unwrap().parse_format.as_deref(),
            Some("json")
        );
        let logfmt = r#"time=2026-07-15T10:00:00Z level=error msg="db down""#;
        assert_eq!(
            parse_log_entry(logfmt, "t")
                .unwrap()
                .parse_format
                .as_deref(),
            Some("logfmt")
        );
    }

    #[test]
    fn test_sniff_level_catches_exception_class_names() {
        // Suffix forms a plain \berror\b can't see (found by the whatbroke
        // metric: JS stack traces sniffed as info and vanished from `errors`).
        for line in [
            "TypeError: Cannot read property 'id' of undefined",
            "DatabaseException in handler",
            "thread 'main' panicked at src/main.rs:4:9",
        ] {
            assert_eq!(
                parse_log_entry(line, "t").unwrap().level.as_deref(),
                Some("error"),
                "line should sniff as error: {line}"
            );
        }
    }

    #[test]
    fn test_sniff_level_catches_failure_prose_and_error_codes() {
        for line in [
            "Cannot find package '@sveltejs/adapter-auto'",
            "ESLint couldn't find an eslint.config.js file.",
            "Unable to resolve dependency tree",
            "ENOENT: no such file or directory",
            "Error [ERR_MODULE_NOT_FOUND]: something",
        ] {
            assert_eq!(
                parse_log_entry(line, "t").unwrap().level.as_deref(),
                Some("error"),
                "line should sniff as error: {line}"
            );
        }
        // Ordinary words starting with 'e' must NOT trip the code pattern.
        for line in [
            "everything is fine",
            "ESLint: 9.39.4",
            "extra logging enabled",
        ] {
            assert_eq!(
                parse_log_entry(line, "t").unwrap().level,
                None,
                "line should not sniff a level: {line}"
            );
        }
    }

    #[test]
    fn test_fallback_entries_are_stamped_raw() {
        // No structured parser claims it → "raw", even when a level was sniffed.
        let entry = parse_log_entry("ERROR something exploded in module foo", "t").unwrap();
        assert_eq!(entry.parse_format.as_deref(), Some("raw"));
        assert_eq!(entry.level.as_deref(), Some("error"));
    }
}
