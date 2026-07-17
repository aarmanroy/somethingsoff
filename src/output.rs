//! Unified output contract (v1).
//!
//! Every command emits one envelope shape on stdout:
//!
//! ```json
//! { "ok": true, "command": "search", "version": "1.0.0",
//!   "generated_at": "...", "elapsed_ms": 12.3,
//!   "sync": { ... } | null-omitted,
//!   "data": { ... },
//!   "meta": { "count": .., "total": .., "limit": .., "offset": ..,
//!             "filters": {..}, "time_range": {..} } | omitted,
//!   "notices": [ {"code","message","hint"} ] | omitted }
//! ```
//!
//! Failures emit (and main.rs exits with `error.exit_code`):
//!
//! ```json
//! { "ok": false, "command": "...", "generated_at": "...",
//!   "error": { "code": "...", "message": "...", "hint": "...",
//!              "exit_code": N } }
//! ```
//!
//! Exit codes: 0 ok · 2 ok-but-zero-results · 3 usage/config ·
//! 4 index (locked/corrupt) · 5 permission/IO · 6 parse · 1 internal.
//!
//! `--format jsonl` strips the envelope: one data record per line
//! (entries for search/get, groups for errors, the single data object for
//! everything else). Status travels via exit code and stderr instead.

use clap::ValueEnum;
use serde::Serialize;
use std::time::Instant;

use crate::schema::to_json_sorted_value;
use crate::sync::SyncReport;

/// Output format shared by all commands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum OutputFormat {
    /// Single JSON envelope (default)
    Json,
    /// One data record per line, envelope stripped
    Jsonl,
}

/// Machine-readable failure category. Stable contract — agents branch on it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    Usage,
    ConfigInvalid,
    NoSources,
    IndexLocked,
    IndexCorrupt,
    PermissionDenied,
    IoError,
    ParseError,
    Internal,
}

impl ErrorCode {
    pub fn exit_code(&self) -> i32 {
        match self {
            ErrorCode::Usage | ErrorCode::ConfigInvalid | ErrorCode::NoSources => 3,
            ErrorCode::IndexLocked | ErrorCode::IndexCorrupt => 4,
            ErrorCode::PermissionDenied | ErrorCode::IoError => 5,
            ErrorCode::ParseError => 6,
            ErrorCode::Internal => 1,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            ErrorCode::Usage => "usage",
            ErrorCode::ConfigInvalid => "config_invalid",
            ErrorCode::NoSources => "no_sources",
            ErrorCode::IndexLocked => "index_locked",
            ErrorCode::IndexCorrupt => "index_corrupt",
            ErrorCode::PermissionDenied => "permission_denied",
            ErrorCode::IoError => "io_error",
            ErrorCode::ParseError => "parse_error",
            ErrorCode::Internal => "internal",
        }
    }
}

/// A typed failure carrying the contract fields. Constructed at failure
/// sites and downcast from `anyhow::Error` in main.rs for the envelope.
#[derive(Debug)]
pub struct CliError {
    pub code: ErrorCode,
    pub message: String,
    pub hint: Option<String>,
}

impl CliError {
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        CliError {
            code,
            message: message.into(),
            hint: None,
        }
    }

    pub fn with_hint(mut self, hint: impl Into<String>) -> Self {
        self.hint = Some(hint.into());
        self
    }
}

impl std::fmt::Display for CliError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for CliError {}

/// Non-fatal, actionable information attached to a successful response.
#[derive(Debug, Clone, Serialize)]
pub struct Notice {
    pub code: String,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hint: Option<String>,
}

/// Envelope builder. Create at command start, feed it sync/notices, emit
/// once with the command's data (+ optional meta).
pub struct Envelope {
    command: &'static str,
    started: Instant,
    sync: Option<SyncReport>,
    notices: Vec<Notice>,
}

impl Envelope {
    pub fn new(command: &'static str) -> Self {
        Envelope {
            command,
            started: Instant::now(),
            sync: None,
            notices: Vec::new(),
        }
    }

    pub fn set_sync(&mut self, report: SyncReport) {
        // Surface a lock-degraded read so agents know results may be stale.
        if report.skipped && report.reason.as_deref() == Some("locked") {
            self.notice(
                "sync_deferred",
                "Another process holds the index lock; results may be stale",
                Some("A `somethingsoff watch` or ingest is running. Results lag by at most its poll interval."),
            );
        }
        if report.migrated {
            self.notice(
                "index_migrated",
                "The index was rebuilt for a schema upgrade (one-time)",
                None,
            );
        }
        self.sync = Some(report);
    }

    pub fn notice(&mut self, code: &str, message: &str, hint: Option<&str>) {
        self.notices.push(Notice {
            code: code.to_string(),
            message: message.to_string(),
            hint: hint.map(String::from),
        });
    }

    pub fn elapsed_ms(&self) -> f64 {
        (self.started.elapsed().as_secs_f64() * 1000.0 * 100.0).round() / 100.0
    }

    /// Serialize the full envelope (sorted keys, deterministic).
    pub fn render(
        &self,
        data: impl Serialize,
        meta: Option<serde_json::Value>,
    ) -> anyhow::Result<String> {
        let mut envelope = serde_json::Map::new();
        envelope.insert("ok".into(), serde_json::Value::Bool(true));
        envelope.insert("command".into(), self.command.into());
        envelope.insert("version".into(), env!("CARGO_PKG_VERSION").into());
        envelope.insert(
            "generated_at".into(),
            chrono::Utc::now()
                .format("%Y-%m-%dT%H:%M:%S%.3fZ")
                .to_string()
                .into(),
        );
        envelope.insert("elapsed_ms".into(), self.elapsed_ms().into());
        if let Some(sync) = &self.sync {
            envelope.insert("sync".into(), serde_json::to_value(sync)?);
        }
        envelope.insert("data".into(), serde_json::to_value(&data)?);
        if let Some(meta) = meta {
            envelope.insert("meta".into(), meta);
        }
        if !self.notices.is_empty() {
            envelope.insert("notices".into(), serde_json::to_value(&self.notices)?);
        }
        Ok(to_json_sorted_value(&serde_json::Value::Object(envelope))?)
    }

    /// Print the envelope to stdout.
    pub fn emit(
        &self,
        data: impl Serialize,
        meta: Option<serde_json::Value>,
    ) -> anyhow::Result<()> {
        println!("{}", self.render(data, meta)?);
        Ok(())
    }

    /// Print one line per record, envelope stripped (`--format jsonl`).
    pub fn emit_jsonl<T: Serialize>(&self, records: &[T]) -> anyhow::Result<()> {
        for record in records {
            println!("{}", to_json_sorted_value(record)?);
        }
        Ok(())
    }
}

/// Render the error envelope for a failure. Used only by main.rs.
pub fn render_error(command: &str, error: &CliError) -> String {
    let hint = error
        .hint
        .clone()
        .map(serde_json::Value::String)
        .unwrap_or(serde_json::Value::Null);
    let body = serde_json::json!({
        "ok": false,
        "command": command,
        "generated_at": chrono::Utc::now().format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string(),
        "error": {
            "code": error.code.as_str(),
            "message": error.message,
            "hint": hint,
            "exit_code": error.code.exit_code(),
        },
    });
    to_json_sorted_value(&body).unwrap_or_else(|_| body.to_string())
}

/// Classify an arbitrary `anyhow::Error` into the contract's error type.
pub fn classify_error(error: &anyhow::Error) -> CliError {
    if let Some(cli) = error.downcast_ref::<CliError>() {
        return CliError {
            code: cli.code,
            message: cli.message.clone(),
            hint: cli.hint.clone(),
        };
    }
    if let Some(log_err) = error.downcast_ref::<crate::error::LogServiceError>() {
        return CliError {
            code: log_err.code(),
            message: log_err.to_string(),
            hint: log_err.hint(),
        };
    }
    let message = error.to_string();
    // Heuristics for errors that bubble up as plain anyhow context strings.
    let code = if message.contains("index lock") || message.contains("LockBusy") {
        ErrorCode::IndexLocked
    } else if message.contains("Permission denied") {
        ErrorCode::PermissionDenied
    } else {
        ErrorCode::Internal
    };
    CliError {
        code,
        message,
        hint: None,
    }
}

/// Standard meta object for list-shaped responses.
#[allow(clippy::too_many_arguments)]
pub fn list_meta(
    count: usize,
    total: usize,
    limit: Option<usize>,
    offset: Option<usize>,
    filters: serde_json::Value,
    time_range: Option<serde_json::Value>,
) -> serde_json::Value {
    let mut meta = serde_json::Map::new();
    meta.insert("count".into(), count.into());
    meta.insert("total".into(), total.into());
    if let Some(limit) = limit {
        meta.insert("limit".into(), limit.into());
    }
    if let Some(offset) = offset {
        meta.insert("offset".into(), offset.into());
    }
    meta.insert("filters".into(), filters);
    if let Some(time_range) = time_range {
        meta.insert("time_range".into(), time_range);
    }
    serde_json::Value::Object(meta)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_exit_code_mapping() {
        assert_eq!(ErrorCode::Usage.exit_code(), 3);
        assert_eq!(ErrorCode::ConfigInvalid.exit_code(), 3);
        assert_eq!(ErrorCode::NoSources.exit_code(), 3);
        assert_eq!(ErrorCode::IndexLocked.exit_code(), 4);
        assert_eq!(ErrorCode::IndexCorrupt.exit_code(), 4);
        assert_eq!(ErrorCode::PermissionDenied.exit_code(), 5);
        assert_eq!(ErrorCode::IoError.exit_code(), 5);
        assert_eq!(ErrorCode::ParseError.exit_code(), 6);
        assert_eq!(ErrorCode::Internal.exit_code(), 1);
    }

    #[test]
    fn test_envelope_shape() {
        let envelope = Envelope::new("search");
        let rendered = envelope
            .render(serde_json::json!({"results": []}), None)
            .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&rendered).unwrap();
        assert_eq!(parsed["ok"], true);
        assert_eq!(parsed["command"], "search");
        assert!(parsed["version"].is_string());
        assert!(parsed["generated_at"].is_string());
        assert!(parsed["elapsed_ms"].is_number());
        assert!(parsed["data"]["results"].is_array());
        // Omitted when absent:
        assert!(parsed.get("sync").is_none());
        assert!(parsed.get("meta").is_none());
        assert!(parsed.get("notices").is_none());
    }

    #[test]
    fn test_envelope_includes_sync_and_notices() {
        let mut envelope = Envelope::new("stats");
        envelope.set_sync(SyncReport {
            skipped: true,
            reason: Some("locked".to_string()),
            files_checked: 2,
            ingested: 0,
            failed: 0,
            elapsed_ms: 0.5,
            migrated: false,
        });
        let rendered = envelope.render(serde_json::json!({}), None).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&rendered).unwrap();
        assert_eq!(parsed["sync"]["reason"], "locked");
        assert_eq!(parsed["notices"][0]["code"], "sync_deferred");
        assert!(parsed["notices"][0]["hint"].is_string());
    }

    #[test]
    fn test_error_envelope_shape() {
        let error = CliError::new(ErrorCode::IndexLocked, "Another process holds the lock")
            .with_hint("Stop the running watch, or pass --no-sync");
        let rendered = render_error("search", &error);
        let parsed: serde_json::Value = serde_json::from_str(&rendered).unwrap();
        assert_eq!(parsed["ok"], false);
        assert_eq!(parsed["error"]["code"], "index_locked");
        assert_eq!(parsed["error"]["exit_code"], 4);
        assert!(parsed["error"]["hint"]
            .as_str()
            .unwrap()
            .contains("--no-sync"));
    }

    #[test]
    fn test_classify_anyhow_fallback_is_internal() {
        let err = anyhow::anyhow!("something unexpected");
        let cli = classify_error(&err);
        assert_eq!(cli.code, ErrorCode::Internal);
        assert_eq!(cli.code.exit_code(), 1);
    }

    #[test]
    fn test_classify_preserves_cli_error() {
        let err: anyhow::Error = CliError::new(ErrorCode::Usage, "get requires a selector").into();
        let cli = classify_error(&err);
        assert_eq!(cli.code, ErrorCode::Usage);
    }
}
