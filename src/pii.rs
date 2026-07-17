//! PII (Personally Identifiable Information) redaction module
//!
//! Automatically detects and redacts sensitive data during log ingestion
//! to prevent AI agents from seeing raw passwords, API keys, emails, etc.
//!
//! Redaction happens at the RawLogEntry level (after parsing, before indexing)
//! so PII never enters the Tantivy index.

use once_cell::sync::Lazy;
use regex::{Regex, RegexSet};

use crate::schema::{ErrorInfo, RawLogEntry};

// ---------------------------------------------------------------------------
// Pattern sources (shared by the per-pattern regexes and the RegexSet so the
// cheap pre-check can never drift out of sync with the replacement pass)
// ---------------------------------------------------------------------------

/// Email addresses: user@example.com, user+tag@sub.domain.co.uk
const EMAIL_PAT: &str = r"(?i)\b[A-Za-z0-9._%+\-]+@[A-Za-z0-9.\-]+\.[A-Za-z]{2,}\b";
/// Bearer / basic auth tokens: Bearer sk-abc123, Basic dXNlcjpwYXNz, token=abc123
const AUTH_TOKEN_PAT: &str = r"(?i)(bearer|basic)\s+\S+|(token)\s*[=:]\s*\S+";
/// Passwords in key-value or JSON form: password=secret, "password":"secret", pwd: abc
const PASSWORD_PAT: &str = r#"(?i)(password|passwd|pwd|secret|pass)\s*[:=]\s*\S+"#;
/// API keys: api_key=xxx, apikey xxx, secret_key=xxx, access-key xxx
const API_KEY_PAT: &str =
    r#"(?i)(api[_-]?key|apikey|secret[_-]?key|private[_-]?key|access[_-]?key)\s*[:= ]\s*\S+"#;
/// Credit card numbers: 4111-1111-1111-1111, 4111 1111 1111 1111, 4111111111111111
const CREDIT_CARD_PAT: &str = r"\b\d{4}[\s-]?\d{4}[\s-]?\d{4}[\s-]?\d{4}\b";
/// US Social Security Numbers: 123-45-6789
const SSN_PAT: &str = r"\b\d{3}-\d{2}-\d{4}\b";
/// AWS-style access key IDs (AKIA followed by 16 uppercase alphanumeric)
const AWS_KEY_PAT: &str = r"AKIA[A-Z0-9]{16}";
/// Generic hex tokens that look like secrets (32+ hex chars after key= pattern)
const HEX_SECRET_PAT: &str =
    r#"(?i)(key|token|secret|auth|credential|session)\s*[:=]\s*[a-f0-9]{32,}"#;

// ---------------------------------------------------------------------------
// Compiled regex patterns (allocated once, reused across all invocations)
// ---------------------------------------------------------------------------

static EMAIL_RE: Lazy<Regex> = Lazy::new(|| Regex::new(EMAIL_PAT).unwrap());
static AUTH_TOKEN_RE: Lazy<Regex> = Lazy::new(|| Regex::new(AUTH_TOKEN_PAT).unwrap());
static PASSWORD_RE: Lazy<Regex> = Lazy::new(|| Regex::new(PASSWORD_PAT).unwrap());
static API_KEY_RE: Lazy<Regex> = Lazy::new(|| Regex::new(API_KEY_PAT).unwrap());
static CREDIT_CARD_RE: Lazy<Regex> = Lazy::new(|| Regex::new(CREDIT_CARD_PAT).unwrap());
static SSN_RE: Lazy<Regex> = Lazy::new(|| Regex::new(SSN_PAT).unwrap());
static AWS_KEY_RE: Lazy<Regex> = Lazy::new(|| Regex::new(AWS_KEY_PAT).unwrap());
static HEX_SECRET_RE: Lazy<Regex> = Lazy::new(|| Regex::new(HEX_SECRET_PAT).unwrap());

/// One combined matcher over every PII pattern. `is_match` runs a single DFA
/// pass with no allocation, so the overwhelmingly common "no PII on this line"
/// case skips all eight `replace_all` passes and their intermediate `String`s.
static PII_SET: Lazy<RegexSet> = Lazy::new(|| {
    RegexSet::new([
        EMAIL_PAT,
        AUTH_TOKEN_PAT,
        PASSWORD_PAT,
        API_KEY_PAT,
        CREDIT_CARD_PAT,
        SSN_PAT,
        AWS_KEY_PAT,
        HEX_SECRET_PAT,
    ])
    .unwrap()
});

// ---------------------------------------------------------------------------
// Redaction logic
// ---------------------------------------------------------------------------

/// Redact PII from a string, replacing matched patterns with safe placeholders.
fn redact_string(input: &str) -> String {
    let mut result = input.to_string();

    // Order matters: most specific patterns first to avoid double-replacing

    // 1. Credit cards (before generic number patterns)
    result = CREDIT_CARD_RE
        .replace_all(&result, "[CC REDACTED]")
        .to_string();

    // 2. SSNs
    result = SSN_RE.replace_all(&result, "[SSN REDACTED]").to_string();

    // 3. AWS access keys
    result = AWS_KEY_RE
        .replace_all(&result, "[AWS KEY REDACTED]")
        .to_string();

    // 4. Auth tokens (Bearer, Basic, token=)
    result = AUTH_TOKEN_RE
        .replace_all(&result, |caps: &regex::Captures| {
            if let Some(keyword) = caps.get(1) {
                format!("{} [REDACTED]", keyword.as_str())
            } else if let Some(keyword) = caps.get(2) {
                format!("{} [REDACTED]", keyword.as_str())
            } else {
                "[REDACTED]".to_string()
            }
        })
        .to_string();

    // 5. Hex secrets (long hex strings after key/token keywords)
    result = HEX_SECRET_RE
        .replace_all(&result, "$1 [REDACTED]")
        .to_string();

    // 6. API keys
    result = API_KEY_RE.replace_all(&result, "$1 [REDACTED]").to_string();

    // 7. Passwords
    result = PASSWORD_RE
        .replace_all(&result, "$1 [REDACTED]")
        .to_string();

    // 8. Email addresses (last — emails can appear inside other patterns)
    result = EMAIL_RE
        .replace_all(&result, "[EMAIL REDACTED]")
        .to_string();

    result
}

/// Redact an owned string, returning it untouched (no allocation) when no PII
/// pattern matches — the common case on the ingest hot path. Only strings that
/// actually contain PII pay for the `replace_all` chain.
fn redact_owned(s: String) -> String {
    if PII_SET.is_match(&s) {
        redact_string(&s)
    } else {
        s
    }
}

/// Redact PII from an `ErrorInfo` struct.
fn redact_error_info(error: ErrorInfo) -> ErrorInfo {
    ErrorInfo {
        name: error.name.map(redact_owned),
        message: error.message.map(redact_owned),
        code: error.code, // codes are not PII
    }
}

/// Recursively redact string values inside an attribute JSON value.
fn redact_json_value(value: serde_json::Value) -> serde_json::Value {
    match value {
        serde_json::Value::String(s) => serde_json::Value::String(redact_owned(s)),
        serde_json::Value::Array(items) => {
            serde_json::Value::Array(items.into_iter().map(redact_json_value).collect())
        }
        serde_json::Value::Object(map) => serde_json::Value::Object(
            map.into_iter()
                .map(|(k, v)| (k, redact_json_value(v)))
                .collect(),
        ),
        other => other,
    }
}

/// Redact PII from a `RawLogEntry`.
///
/// This is the main entry point called from the ingest pipeline.
/// Only text fields that can contain free-form PII are redacted:
/// - `message`: most likely to contain PII
/// - `error.name` and `error.message`: error messages often leak credentials
/// - `route`: may contain query parameters with tokens
/// - `extra` attribute values: arbitrary app fields can carry anything
///
/// Structured fields like `level`, `source`, `method`, `status_code` are safe
/// and left untouched. `user_id` is kept as-is because it's an application-level
/// identifier, not raw PII.
///
/// If the global `--no-redact` flag is set, the entry is returned unchanged.
pub fn redact_raw_entry(raw: RawLogEntry) -> RawLogEntry {
    // Skip redaction when explicitly disabled via --no-redact
    if crate::is_no_redact() {
        return raw;
    }

    RawLogEntry {
        message: raw.message.map(redact_owned),
        error: raw.error.map(redact_error_info),
        route: raw.route.map(redact_owned),
        extra: raw
            .extra
            .into_iter()
            .map(|(k, v)| (k, redact_json_value(v)))
            .collect(),
        // Structured fields — not redacted
        timestamp: raw.timestamp,
        level: raw.level,
        source: raw.source,
        request_id: raw.request_id,
        user_id: raw.user_id,
        method: raw.method,
        status_code: raw.status_code,
        duration_ms: raw.duration_ms,
        source_file: raw.source_file,
        line_number: raw.line_number,
        parse_format: raw.parse_format,
        ingest_position: raw.ingest_position,
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // ---- Individual pattern tests ----

    #[test]
    fn test_redact_email() {
        assert_eq!(
            redact_string("User user@example.com logged in"),
            "User [EMAIL REDACTED] logged in"
        );
        assert_eq!(
            redact_string("Contact: admin@company.co.uk"),
            "Contact: [EMAIL REDACTED]"
        );
        assert_eq!(
            redact_string("Email: user+tag@sub.domain.org"),
            "Email: [EMAIL REDACTED]"
        );
    }

    #[test]
    fn test_redact_bearer_token() {
        assert_eq!(
            redact_string("Authorization: Bearer sk-abc123def456"),
            "Authorization: Bearer [REDACTED]"
        );
        assert_eq!(redact_string("token=abc123secret"), "token [REDACTED]");
        assert_eq!(
            redact_string("Bearer eyJhbGciOiJIUzI1NiJ9.payload"),
            "Bearer [REDACTED]"
        );
    }

    #[test]
    fn test_redact_password() {
        assert_eq!(redact_string("password=secret123"), "password [REDACTED]");
        assert_eq!(
            redact_string("User logged in with password: myP@ssw0rd!"),
            "User logged in with password [REDACTED]"
        );
        assert_eq!(redact_string("passwd=hunter2"), "passwd [REDACTED]");
    }

    #[test]
    fn test_redact_api_key() {
        assert_eq!(
            redact_string("api_key=sk_live_abc123def456"),
            "api_key [REDACTED]"
        );
        assert_eq!(redact_string("apikey sk_live_abc123"), "apikey [REDACTED]");
        assert_eq!(
            redact_string("secret_key=abc123xyz"),
            "secret_key [REDACTED]"
        );
    }

    #[test]
    fn test_redact_credit_card() {
        assert_eq!(
            redact_string("Card: 4111-1111-1111-1111 charged"),
            "Card: [CC REDACTED] charged"
        );
        assert_eq!(
            redact_string("Card 4111111111111111 processed"),
            "Card [CC REDACTED] processed"
        );
        assert_eq!(
            redact_string("CC 4111 1111 1111 1111 valid"),
            "CC [CC REDACTED] valid"
        );
    }

    #[test]
    fn test_redact_ssn() {
        assert_eq!(
            redact_string("SSN: 123-45-6789 verified"),
            "SSN: [SSN REDACTED] verified"
        );
    }

    #[test]
    fn test_redact_aws_key() {
        assert_eq!(
            redact_string("Using key AKIAIOSFODNN7EXAMPLE for access"),
            "Using key [AWS KEY REDACTED] for access"
        );
    }

    #[test]
    fn test_redact_hex_secret() {
        // "token=" is caught by AUTH_TOKEN_RE before HEX_SECRET_RE
        assert_eq!(
            redact_string("token=abc123def456abc123def456abc123def456"),
            "token [REDACTED]"
        );
        // "secret=" is caught by HEX_SECRET_RE
        assert_eq!(
            redact_string("secret=a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4"),
            "secret [REDACTED]"
        );
        // "credential=" pattern
        assert_eq!(
            redact_string("credential=a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4"),
            "credential [REDACTED]"
        );
    }

    // ---- Combined pattern tests ----

    #[test]
    fn test_redact_multiple_patterns() {
        let input =
            "User user@test.com logged in with password=secret123 using api_key=sk_live_abc";
        let result = redact_string(input);
        assert!(result.contains("[EMAIL REDACTED]"), "should redact email");
        assert!(
            result.contains("password [REDACTED]"),
            "should redact password"
        );
        assert!(
            result.contains("api_key [REDACTED]"),
            "should redact api_key"
        );
    }

    #[test]
    fn test_no_redaction_for_clean_data() {
        let input = "Request completed successfully in 150ms";
        assert_eq!(redact_string(input), input);
    }

    #[test]
    fn test_no_redaction_for_log_levels() {
        let input = "level=error source=backend";
        assert_eq!(redact_string(input), input);
    }

    // ---- RawLogEntry redaction tests ----

    #[test]
    fn test_redact_raw_entry_message() {
        let raw = RawLogEntry {
            timestamp: Some("2026-03-31T10:00:00.000Z".to_string()),
            level: Some("info".to_string()),
            message: Some("Login by user@example.com with password=abc123".to_string()),
            source: Some("backend".to_string()),
            request_id: Some("req-123".to_string()),
            user_id: Some("user-456".to_string()),
            route: None,
            method: None,
            status_code: None,
            duration_ms: None,
            error: None,
            source_file: None,
            line_number: None,
            extra: Default::default(),
            parse_format: None,
            ingest_position: None,
        };

        let redacted = redact_raw_entry(raw);

        assert_eq!(
            redacted.timestamp,
            Some("2026-03-31T10:00:00.000Z".to_string())
        );
        assert_eq!(redacted.level, Some("info".to_string()));
        assert_eq!(redacted.source, Some("backend".to_string()));
        assert_eq!(redacted.request_id, Some("req-123".to_string()));
        assert_eq!(redacted.user_id, Some("user-456".to_string()));

        let msg = redacted.message.unwrap();
        assert!(
            msg.contains("[EMAIL REDACTED]"),
            "message should have email redacted"
        );
        assert!(
            msg.contains("password [REDACTED]"),
            "message should have password redacted"
        );
        assert!(!msg.contains("user@example.com"), "email should be gone");
        assert!(!msg.contains("abc123"), "password value should be gone");
    }

    #[test]
    fn test_redact_raw_entry_error() {
        let raw = RawLogEntry {
            timestamp: Some("2026-03-31T10:00:00.000Z".to_string()),
            level: Some("error".to_string()),
            message: None,
            source: Some("backend".to_string()),
            request_id: None,
            user_id: None,
            route: None,
            method: None,
            status_code: None,
            duration_ms: None,
            error: Some(ErrorInfo {
                name: Some("AuthError".to_string()),
                message: Some("Token Bearer sk-abc123 is invalid for user@test.com".to_string()),
                code: Some("AUTH_001".to_string()),
            }),
            source_file: None,
            line_number: None,
            extra: Default::default(),
            parse_format: None,
            ingest_position: None,
        };

        let redacted = redact_raw_entry(raw);
        let error = redacted.error.unwrap();

        assert_eq!(error.name, Some("AuthError".to_string()));
        assert_eq!(error.code, Some("AUTH_001".to_string()));

        let err_msg = error.message.unwrap();
        assert!(
            err_msg.contains("Bearer [REDACTED]"),
            "error message should redact bearer token"
        );
        assert!(
            err_msg.contains("[EMAIL REDACTED]"),
            "error message should redact email"
        );
        assert!(
            !err_msg.contains("sk-abc123"),
            "bearer token value should be gone"
        );
    }

    #[test]
    fn test_redact_raw_entry_route_with_query_params() {
        let raw = RawLogEntry {
            timestamp: Some("2026-03-31T10:00:00.000Z".to_string()),
            level: Some("info".to_string()),
            message: Some("Request processed".to_string()),
            source: Some("backend".to_string()),
            request_id: None,
            user_id: None,
            route: Some("/api/reset?session=abc123def456abc123def456abc123def456".to_string()),
            method: Some("POST".to_string()),
            status_code: Some(200),
            duration_ms: Some(50.0),
            error: None,
            source_file: None,
            line_number: None,
            extra: Default::default(),
            parse_format: None,
            ingest_position: None,
        };

        let redacted = redact_raw_entry(raw);

        assert_eq!(redacted.method, Some("POST".to_string()));
        assert_eq!(redacted.status_code, Some(200));
        assert_eq!(redacted.duration_ms, Some(50.0));

        let route = redacted.route.unwrap();
        assert!(
            route.contains("session [REDACTED]"),
            "route should redact session param"
        );
        assert!(
            !route.contains("abc123def456"),
            "session value should be gone"
        );
    }

    #[test]
    fn test_redact_raw_entry_preserves_structured_fields() {
        let raw = RawLogEntry {
            timestamp: Some("2026-03-31T10:00:00.000Z".to_string()),
            level: Some("error".to_string()),
            message: Some("Simple error".to_string()),
            source: Some("backend".to_string()),
            request_id: Some("req-abc".to_string()),
            user_id: Some("user-123".to_string()),
            route: Some("/api/health".to_string()),
            method: Some("GET".to_string()),
            status_code: Some(500),
            duration_ms: Some(12.5),
            error: None,
            source_file: Some("src/routes/api.ts".to_string()),
            line_number: Some(42),
            extra: Default::default(),
            parse_format: None,
            ingest_position: None,
        };

        let redacted = redact_raw_entry(raw);

        // All structured fields unchanged
        assert_eq!(
            redacted.timestamp,
            Some("2026-03-31T10:00:00.000Z".to_string())
        );
        assert_eq!(redacted.level, Some("error".to_string()));
        assert_eq!(redacted.source, Some("backend".to_string()));
        assert_eq!(redacted.request_id, Some("req-abc".to_string()));
        assert_eq!(redacted.user_id, Some("user-123".to_string()));
        assert_eq!(redacted.method, Some("GET".to_string()));
        assert_eq!(redacted.status_code, Some(500));
        assert_eq!(redacted.duration_ms, Some(12.5));
        assert_eq!(redacted.source_file, Some("src/routes/api.ts".to_string()));
        assert_eq!(redacted.line_number, Some(42));

        // Clean message/route unchanged
        assert_eq!(redacted.message, Some("Simple error".to_string()));
        assert_eq!(redacted.route, Some("/api/health".to_string()));
    }

    // ---- Edge case tests ----

    #[test]
    fn test_redact_empty_string() {
        assert_eq!(redact_string(""), "");
    }

    #[test]
    fn test_redact_none_fields_unchanged() {
        let raw = RawLogEntry {
            timestamp: None,
            level: None,
            message: None,
            source: None,
            request_id: None,
            user_id: None,
            route: None,
            method: None,
            status_code: None,
            duration_ms: None,
            error: None,
            source_file: None,
            line_number: None,
            extra: Default::default(),
            parse_format: None,
            ingest_position: None,
        };

        let redacted = redact_raw_entry(raw);
        assert!(redacted.timestamp.is_none());
        assert!(redacted.message.is_none());
    }

    #[test]
    fn test_redact_preserves_case_in_replacement() {
        // "Bearer" should be preserved in "Bearer [REDACTED]"
        let result = redact_string("Bearer sk_test_abc123");
        assert!(result.starts_with("Bearer [REDACTED]"));
    }

    #[test]
    fn test_no_redact_flag_bypasses_redaction() {
        // When no-redact is enabled, raw entry should pass through unchanged
        crate::set_no_redact(true);

        let raw = RawLogEntry {
            timestamp: Some("2026-03-31T10:00:00.000Z".to_string()),
            level: Some("info".to_string()),
            message: Some("Login by user@example.com with password=abc123".to_string()),
            source: Some("backend".to_string()),
            request_id: None,
            user_id: None,
            route: None,
            method: None,
            status_code: None,
            duration_ms: None,
            error: None,
            source_file: None,
            line_number: None,
            extra: Default::default(),
            parse_format: None,
            ingest_position: None,
        };

        let redacted = redact_raw_entry(raw.clone());

        // Should be completely unchanged
        assert_eq!(redacted.message, raw.message);

        // Reset
        crate::set_no_redact(false);
    }
}
