//! Schema definitions for log entries and Tantivy index
//!
//! This module defines the core data structures and Tantivy schema
//! for the log indexing system. Determinism is key - same input always
//! produces same output.

use crate::error::LogServiceError;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use tantivy::schema::*;

/// Version of the on-disk index schema. Bump when `create_schema()`, field
/// aliasing, or log_id inputs change; a mismatch triggers a transparent
/// rebuild (sources are on disk, so this is invisible apart from one slower
/// query).
///
/// v4: added the `timestamp_ms` numeric fast field for O(fast-field) time-range
/// filtering, and normalized Python comma-millis timestamps (which changes both
/// display and `log_id` for those docs — hence the reindex).
///
/// v5: added the `parse_format` fast field stamping each entry with the parser
/// that understood it ("raw" = lossless fallback), powering `stats --by-format`
/// and `search --parse-format`.
///
/// v6: log_id for entries with no intrinsic timestamp is salted with the
/// ingest position (file:byte-offset), so identical raw lines keep honest
/// counts instead of collapsing, while re-reads still dedup by position.
pub const SCHEMA_VERSION: u32 = 6;

/// Log entry as stored in the index and returned from searches.
///
/// Per PRD: Same input → same output, always.
/// Keys are serialized in sorted order for determinism.
/// All optional fields are serialized as null when None for determinism.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct LogEntry {
    /// SHA-256 hash (first 16 chars) - stable unique ID
    pub log_id: String,
    /// ISO8601 UTC with millis: 2024-01-15T10:30:00.123Z
    pub timestamp: String,
    /// Log level: debug|info|warn|error (always lowercase)
    pub level: String,
    /// Source: frontend|backend|telemetry
    pub source: String,
    /// Log message
    pub message: String,
    /// Optional request ID for request tracing
    pub request_id: Option<String>,
    /// Optional user ID for user context
    pub user_id: Option<String>,
    /// Optional route (e.g., /api/auth)
    pub route: Option<String>,
    /// Optional HTTP method (GET, POST, etc.)
    pub method: Option<String>,
    /// Optional HTTP status code
    pub status_code: Option<u16>,
    /// Optional duration in milliseconds
    pub duration_ms: Option<f64>,
    /// Optional error information
    pub error: Option<ErrorInfo>,
    /// Optional source file path
    pub source_file: Option<String>,
    /// Optional line number
    pub line_number: Option<usize>,
    /// Any JSON fields that don't map to the core schema (preserved, stored,
    /// and full-text searchable; never silently dropped)
    pub attributes: Option<BTreeMap<String, serde_json::Value>>,
    /// Which parser understood this line at ingest ("json", "logfmt",
    /// "syslog", ...); "raw" = no structured parser claimed it and the line
    /// was captured by the lossless fallback. High raw share in an index
    /// means field filters (level, request_id, ...) see only part of the data.
    pub parse_format: String,
}

/// Error information embedded in log entries
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ErrorInfo {
    /// Error name/type
    pub name: Option<String>,
    /// Error message
    pub message: Option<String>,
    /// Error code if available
    pub code: Option<String>,
}

/// Raw log entry for ingestion (before normalization and ID generation)
#[derive(Debug, Clone, Deserialize, Default)]
pub struct RawLogEntry {
    pub timestamp: Option<String>,
    pub level: Option<String>,
    pub message: Option<String>,
    pub source: Option<String>,
    pub request_id: Option<String>,
    pub user_id: Option<String>,
    pub route: Option<String>,
    pub method: Option<String>,
    pub status_code: Option<u16>,
    pub duration_ms: Option<f64>,
    pub error: Option<ErrorInfo>,
    pub source_file: Option<String>,
    pub line_number: Option<usize>,
    /// Unrecognized fields, preserved verbatim (BTreeMap for determinism)
    #[serde(skip)]
    pub extra: BTreeMap<String, serde_json::Value>,
    /// Set by `parse_log_entry` to the matched parser's name; never taken
    /// from input (an input field named "parse_format" lands in `extra`).
    #[serde(skip)]
    pub parse_format: Option<String>,
    /// Stable ingest position ("<file>:<byte-offset>"), set by the ingest
    /// paths. Salts log_id for entries with no intrinsic timestamp so
    /// identical lines at different positions stay distinct documents while
    /// re-reads of the same position still dedup. Never taken from input.
    #[serde(skip)]
    pub ingest_position: Option<String>,
}

impl RawLogEntry {
    /// Map an arbitrary JSON object into a RawLogEntry.
    ///
    /// Handles the aliases real-world loggers emit (camelCase `requestId`,
    /// `msg`, `@timestamp`, epoch times, numbers-as-strings) and routes
    /// everything unrecognized into `extra` instead of dropping it.
    /// Returns None for non-object values.
    pub fn from_value(value: serde_json::Value) -> Option<RawLogEntry> {
        let serde_json::Value::Object(map) = value else {
            return None;
        };
        let mut map: BTreeMap<String, serde_json::Value> = map.into_iter().collect();

        let timestamp = take_timestamp(&mut map);
        let level = take_string(&mut map, &["level", "severity", "lvl"]);
        let message = take_string(&mut map, &["message", "msg"]);
        let source = take_string(&mut map, &["source"]);
        let request_id = take_string(
            &mut map,
            &[
                "request_id",
                "requestId",
                "reqId",
                "correlation_id",
                "correlationId",
            ],
        );
        let user_id = take_string(&mut map, &["user_id", "userId", "uid"]);
        let route = take_string(&mut map, &["route", "path", "url", "endpoint"]);
        let method = take_string(&mut map, &["method"]);
        // "status" is consumed only when numeric — a text status ("ok")
        // stays in attributes rather than corrupting status_code.
        let status_code = take_number(&mut map, &["status_code", "statusCode", "status"])
            .and_then(|n| u16::try_from(n as i64).ok());
        let duration_ms = take_number(
            &mut map,
            &["duration_ms", "durationMs", "duration", "response_time_ms"],
        );
        let error = take_error(&mut map);
        let source_file = take_string(&mut map, &["source_file", "sourceFile"]);
        let line_number = take_number(&mut map, &["line_number", "lineNumber", "line"])
            .and_then(|n| usize::try_from(n as i64).ok());

        Some(RawLogEntry {
            timestamp,
            level,
            message,
            source,
            request_id,
            user_id,
            route,
            method,
            status_code,
            duration_ms,
            error,
            source_file,
            line_number,
            extra: map,
            parse_format: None,
            ingest_position: None,
        })
    }
}

/// Take the first present key as a string (numbers are stringified).
fn take_string(map: &mut BTreeMap<String, serde_json::Value>, keys: &[&str]) -> Option<String> {
    for key in keys {
        match map.get(*key) {
            Some(serde_json::Value::String(_)) => {
                if let Some(serde_json::Value::String(s)) = map.remove(*key) {
                    return Some(s);
                }
            }
            Some(serde_json::Value::Number(_)) => {
                if let Some(serde_json::Value::Number(n)) = map.remove(*key) {
                    return Some(n.to_string());
                }
            }
            // Wrong type (or null): leave it for `extra`.
            _ => {}
        }
    }
    None
}

/// Take the first present key coercible to a number (numeric strings count).
fn take_number(map: &mut BTreeMap<String, serde_json::Value>, keys: &[&str]) -> Option<f64> {
    for key in keys {
        let coerced = match map.get(*key) {
            Some(serde_json::Value::Number(n)) => n.as_f64(),
            Some(serde_json::Value::String(s)) => s.trim().parse::<f64>().ok(),
            _ => None,
        };
        if let Some(n) = coerced {
            map.remove(*key);
            return Some(n);
        }
    }
    None
}

/// Take a timestamp from the usual keys: RFC3339-ish strings pass through
/// verbatim (normalized later in `from_raw`); epoch numbers are converted.
fn take_timestamp(map: &mut BTreeMap<String, serde_json::Value>) -> Option<String> {
    for key in ["timestamp", "time", "ts", "@timestamp", "datetime"] {
        match map.get(key) {
            Some(serde_json::Value::String(_)) => {
                if let Some(serde_json::Value::String(s)) = map.remove(key) {
                    return Some(s);
                }
            }
            Some(serde_json::Value::Number(n)) => {
                if let Some(converted) = n.as_f64().and_then(epoch_to_rfc3339) {
                    map.remove(key);
                    return Some(converted);
                }
            }
            _ => {}
        }
    }
    None
}

/// Convert an epoch number (seconds or milliseconds, by magnitude) to an
/// ISO8601 string.
fn epoch_to_rfc3339(epoch: f64) -> Option<String> {
    let millis = if epoch >= 1e12 {
        epoch // already milliseconds
    } else {
        epoch * 1000.0
    };
    let dt = chrono::DateTime::from_timestamp_millis(millis as i64)?;
    Some(dt.format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string())
}

/// Take `error` as either an object ({name, message, code}) or a string.
fn take_error(map: &mut BTreeMap<String, serde_json::Value>) -> Option<ErrorInfo> {
    match map.get("error") {
        Some(serde_json::Value::Object(_)) => {
            // Tolerant parse: pull known keys, ignore the rest (stack is
            // handled by the JSON parser before this point).
            if let Some(serde_json::Value::Object(obj)) = map.remove("error") {
                let get_str = |k: &str| obj.get(k).and_then(|v| v.as_str()).map(|s| s.to_string());
                return Some(ErrorInfo {
                    name: get_str("name"),
                    message: get_str("message"),
                    code: get_str("code"),
                });
            }
            None
        }
        Some(serde_json::Value::String(_)) => {
            if let Some(serde_json::Value::String(s)) = map.remove("error") {
                return Some(ErrorInfo {
                    name: None,
                    message: Some(s),
                    code: None,
                });
            }
            None
        }
        _ => None,
    }
}

/// Parse a timestamp string to a UTC `DateTime`, the single source of truth for
/// which timestamp formats we accept. Unparseable inputs return None.
///
/// Accepted forms:
/// - RFC3339 with an explicit timezone (e.g. `...Z`, `...+00:00`)
/// - naive `T`- or space-separated datetimes with optional fractional seconds,
///   assumed UTC (e.g. `2026-07-15T08:00:00.013`, `2026-07-15 08:00:00`)
/// - the same naive forms with a **comma** before the millis — Python's
///   `logging` default (`2026-07-15 08:00:00,013`). Chrono's `%.f` only parses a
///   dot, so we swap the first comma for a dot before the naive attempts.
fn parse_timestamp_to_utc(ts: &str) -> Option<chrono::DateTime<chrono::Utc>> {
    // RFC3339 with timezone
    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(ts) {
        return Some(dt.with_timezone(&chrono::Utc));
    }
    // Naive datetime variants (assume UTC). Swap Python's comma-millis for a dot
    // only when present, so the common dotted/whole-second forms allocate nothing.
    let candidate = if ts.contains(',') {
        std::borrow::Cow::Owned(ts.replacen(',', ".", 1))
    } else {
        std::borrow::Cow::Borrowed(ts)
    };
    for format in ["%Y-%m-%dT%H:%M:%S%.f", "%Y-%m-%d %H:%M:%S%.f"] {
        if let Ok(naive) = chrono::NaiveDateTime::parse_from_str(&candidate, format) {
            return Some(naive.and_utc());
        }
    }
    None
}

/// Normalize a timestamp string to canonical `%Y-%m-%dT%H:%M:%S%.3fZ` UTC.
/// Unparseable inputs return None (caller keeps the verbatim string).
pub fn normalize_timestamp(ts: &str) -> Option<String> {
    parse_timestamp_to_utc(ts).map(|dt| dt.format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string())
}

/// Parse a timestamp string to epoch milliseconds, accepting the exact same
/// formats as [`normalize_timestamp`]. Used to populate the `timestamp_ms` fast
/// field at ingest so time-range filters are a numeric column comparison rather
/// than a term-dictionary walk. Unparseable inputs return None (the doc simply
/// gets no `timestamp_ms`, matching its un-normalizable string timestamp).
pub fn parse_timestamp_millis(ts: &str) -> Option<i64> {
    parse_timestamp_to_utc(ts).map(|dt| dt.timestamp_millis())
}

/// Holds field handles for type-safe Tantivy access
pub struct LogFields {
    /// Full-text indexed field for general queries
    pub full_text: Field,
    /// Unique log ID (SHA-256 hash prefix)
    pub log_id: Field,
    /// ISO8601 timestamp
    pub timestamp: Field,
    /// Timestamp as epoch millis (numeric fast field for range filtering)
    pub timestamp_ms: Field,
    /// Log level (lowercase)
    pub level: Field,
    /// Source system
    pub source: Field,
    /// Log message
    pub message: Field,
    /// Request ID for tracing
    pub request_id: Field,
    /// User ID for context
    pub user_id: Field,
    /// API route
    pub route: Field,
    /// HTTP method
    pub method: Field,
    /// HTTP status code
    pub status_code: Field,
    /// Duration in milliseconds
    pub duration_ms: Field,
    /// Error name
    pub error_name: Field,
    /// Error message
    pub error_message: Field,
    /// Source file path
    pub source_file: Field,
    /// Line number
    pub line_number: Field,
    /// Unrecognized JSON fields, stored as a canonical JSON string
    pub attributes: Field,
    /// Parser that understood the line ("raw" = lossless fallback)
    pub parse_format: Field,
}

impl LogFields {
    /// Create LogFields from a schema by looking up field names
    pub fn new(schema: &Schema) -> Result<Self, LogServiceError> {
        Ok(LogFields {
            full_text: schema
                .get_field("full_text")
                .map_err(|_| LogServiceError::Schema("full_text field missing".to_string()))?,
            log_id: schema
                .get_field("log_id")
                .map_err(|_| LogServiceError::Schema("log_id field missing".to_string()))?,
            timestamp: schema
                .get_field("timestamp")
                .map_err(|_| LogServiceError::Schema("timestamp field missing".to_string()))?,
            timestamp_ms: schema
                .get_field("timestamp_ms")
                .map_err(|_| LogServiceError::Schema("timestamp_ms field missing".to_string()))?,
            level: schema
                .get_field("level")
                .map_err(|_| LogServiceError::Schema("level field missing".to_string()))?,
            source: schema
                .get_field("source")
                .map_err(|_| LogServiceError::Schema("source field missing".to_string()))?,
            message: schema
                .get_field("message")
                .map_err(|_| LogServiceError::Schema("message field missing".to_string()))?,
            request_id: schema
                .get_field("request_id")
                .map_err(|_| LogServiceError::Schema("request_id field missing".to_string()))?,
            user_id: schema
                .get_field("user_id")
                .map_err(|_| LogServiceError::Schema("user_id field missing".to_string()))?,
            route: schema
                .get_field("route")
                .map_err(|_| LogServiceError::Schema("route field missing".to_string()))?,
            method: schema
                .get_field("method")
                .map_err(|_| LogServiceError::Schema("method field missing".to_string()))?,
            status_code: schema
                .get_field("status_code")
                .map_err(|_| LogServiceError::Schema("status_code field missing".to_string()))?,
            duration_ms: schema
                .get_field("duration_ms")
                .map_err(|_| LogServiceError::Schema("duration_ms field missing".to_string()))?,
            error_name: schema
                .get_field("error_name")
                .map_err(|_| LogServiceError::Schema("error_name field missing".to_string()))?,
            error_message: schema
                .get_field("error_message")
                .map_err(|_| LogServiceError::Schema("error_message field missing".to_string()))?,
            source_file: schema
                .get_field("source_file")
                .map_err(|_| LogServiceError::Schema("source_file field missing".to_string()))?,
            line_number: schema
                .get_field("line_number")
                .map_err(|_| LogServiceError::Schema("line_number field missing".to_string()))?,
            attributes: schema
                .get_field("attributes")
                .map_err(|_| LogServiceError::Schema("attributes field missing".to_string()))?,
            parse_format: schema
                .get_field("parse_format")
                .map_err(|_| LogServiceError::Schema("parse_format field missing".to_string()))?,
        })
    }
}

/// Create the Tantivy schema for log entries
///
/// Schema fields:
/// - log_id: Indexed for fast lookup by hash
/// - timestamp: Stored for display, text for range queries
/// - level: Indexed for filtering
/// - source: Indexed for filtering
/// - message: Text indexed for full-text search
/// - All optional fields stored and indexed as appropriate
pub fn create_schema() -> Schema {
    let mut schema_builder = Schema::builder();

    // Full-text field combining message and other searchable content
    schema_builder.add_text_field("full_text", TEXT | STORED);

    // Core fields - always present
    schema_builder.add_text_field("log_id", STRING | STORED);
    schema_builder.add_text_field("timestamp", STRING | STORED | FAST);
    // Epoch-millis mirror of `timestamp`, indexed as a numeric fast field so
    // `--last`/`--start`/`--end` filters are a columnar range comparison
    // (like `status_code` below) instead of a term-dictionary walk. Not STORED:
    // display always uses the canonical string `timestamp`.
    schema_builder.add_u64_field("timestamp_ms", INDEXED | FAST);
    schema_builder.add_text_field("level", STRING | STORED | FAST);
    schema_builder.add_text_field("source", STRING | STORED | FAST);
    schema_builder.add_text_field("message", TEXT | STORED);

    // Optional context fields. user_id and route are FAST so `stats --by-user`
    // and `--by-route` aggregate over the columnar store instead of reading
    // every document out of the compressed doc store.
    schema_builder.add_text_field("request_id", STRING | STORED);
    schema_builder.add_text_field("user_id", STRING | STORED | FAST);
    schema_builder.add_text_field("route", STRING | STORED | FAST);
    schema_builder.add_text_field("method", STRING | STORED);

    // Numeric fields (INDEXED | FAST enables range filters like --status)
    schema_builder.add_u64_field("status_code", STORED | INDEXED | FAST);
    schema_builder.add_f64_field("duration_ms", STORED | INDEXED | FAST);

    // Error fields (flattened for indexing)
    schema_builder.add_text_field("error_name", STRING | STORED);
    schema_builder.add_text_field("error_message", TEXT | STORED);

    // Source location fields
    schema_builder.add_text_field("source_file", STRING | STORED);
    schema_builder.add_u64_field("line_number", STORED);

    // Unrecognized JSON fields, stored as canonical JSON (searchable via
    // full_text, which gets "key value" pairs appended at upsert)
    schema_builder.add_text_field("attributes", STORED);

    // Which parser understood the line ("raw" = lossless fallback). FAST so
    // `stats --by-format` aggregates over the columnar store like `level`.
    schema_builder.add_text_field("parse_format", STRING | STORED | FAST);

    schema_builder.build()
}

/// Generate a deterministic log_id from log entry fields
///
/// Hash input: timestamp + level + source + message + request_id, plus the
/// ingest position for entries whose timestamp is synthetic (see
/// `LogEntry::from_raw`). Output: first 16 hex characters of SHA-256.
pub fn generate_log_id(
    timestamp: &str,
    level: &str,
    source: &str,
    message: &str,
    request_id: Option<&str>,
    position: Option<&str>,
) -> String {
    let mut hasher = Sha256::new();
    hasher.update(timestamp.as_bytes());
    hasher.update(b"|");
    hasher.update(level.as_bytes());
    hasher.update(b"|");
    hasher.update(source.as_bytes());
    hasher.update(b"|");
    hasher.update(message.as_bytes());
    hasher.update(b"|");
    hasher.update(request_id.unwrap_or("").as_bytes());
    if let Some(position) = position {
        hasher.update(b"|");
        hasher.update(position.as_bytes());
    }

    let result = hasher.finalize();
    hex::encode(&result[..8]) // First 8 bytes = 16 hex chars
}

/// Normalize log level to lowercase
pub fn normalize_level(level: &str) -> String {
    level.to_lowercase()
}

/// Normalize source name to lowercase
pub fn normalize_source(source: &str) -> String {
    source.to_lowercase()
}

impl LogEntry {
    /// Create a LogEntry from a RawLogEntry with normalization and ID generation
    pub fn from_raw(raw: RawLogEntry, default_source: &str) -> Self {
        // Entries without an intrinsic timestamp get ingest wall-clock time;
        // their content alone can't distinguish repeats, so the log_id is
        // additionally salted with the stable ingest position (when known):
        // identical lines keep honest counts, re-reads still dedup.
        let has_own_timestamp = raw.timestamp.is_some();
        let timestamp = raw
            .timestamp
            .map(|ts| normalize_timestamp(&ts).unwrap_or(ts))
            .unwrap_or_else(|| {
                chrono::Utc::now()
                    .format("%Y-%m-%dT%H:%M:%S%.3fZ")
                    .to_string()
            });
        let level = normalize_level(&raw.level.unwrap_or_else(|| "info".to_string()));
        let source = normalize_source(&raw.source.unwrap_or_else(|| default_source.to_string()));
        let message = raw.message.unwrap_or_default();
        let request_id = raw.request_id;

        // Identity: entries with their own timestamp are content-keyed (same
        // structured line always dedups). Entries without one get a synthetic
        // ingest-time timestamp that varies per pass, so it must NOT enter
        // the hash — their identity is position + content instead, which is
        // stable across re-reads (replay dedup) yet distinct per occurrence
        // (honest counts). With no position either (hand-built entries),
        // identity degrades to content-only.
        let (id_timestamp, position_salt) = if has_own_timestamp {
            (timestamp.as_str(), None)
        } else {
            ("", raw.ingest_position.as_deref())
        };
        let log_id = generate_log_id(
            id_timestamp,
            &level,
            &source,
            &message,
            request_id.as_deref(),
            position_salt,
        );

        LogEntry {
            log_id,
            timestamp,
            level,
            source,
            message,
            request_id,
            user_id: raw.user_id,
            route: raw.route,
            method: raw.method,
            status_code: raw.status_code,
            duration_ms: raw.duration_ms,
            error: raw.error,
            source_file: raw.source_file,
            line_number: raw.line_number,
            attributes: if raw.extra.is_empty() {
                None
            } else {
                Some(raw.extra)
            },
            // Unstamped entries (hand-built RawLogEntry, e.g. in tests) count
            // as "raw": unknown parse origin must not inflate structured rates.
            parse_format: raw.parse_format.unwrap_or_else(|| "raw".to_string()),
        }
    }

    /// Serialize to JSON with sorted keys (deterministic output per PRD)
    pub fn to_json_sorted(&self) -> serde_json::Result<String> {
        // Serialize to Value, sort keys alphabetically, then serialize to string
        let value = serde_json::to_value(self)?;
        sort_json_value_to_string(&value)
    }
}

/// Format a list of log entries according to compact and fields selection
pub fn format_log_entry(
    entry: &LogEntry,
    compact: bool,
    fields: Option<&[String]>,
) -> serde_json::Result<serde_json::Value> {
    let val = serde_json::to_value(entry)?;

    if let serde_json::Value::Object(mut map) = val {
        // Filter fields if specified
        if let Some(fields) = fields {
            let mut new_map = serde_json::Map::new();
            for field in fields {
                if let Some(v) = map.remove(field) {
                    new_map.insert(field.clone(), v);
                }
            }
            map = new_map;
        }

        // Remove nulls if compact
        if compact {
            map.retain(|_, v| !v.is_null());
        }

        Ok(serde_json::Value::Object(map))
    } else {
        Ok(val)
    }
}

/// Format a list of log entries according to compact and fields selection
pub fn format_log_entries(
    entries: &[LogEntry],
    compact: bool,
    fields: Option<&[String]>,
) -> serde_json::Result<serde_json::Value> {
    let mut json_entries = Vec::new();

    for entry in entries {
        json_entries.push(format_log_entry(entry, compact, fields)?);
    }

    Ok(serde_json::Value::Array(json_entries))
}

/// Generic function to serialize any T: Serialize with sorted keys (deterministic output)
///
/// This is the core output function for all CLI commands to ensure PRD compliance:
/// - Keys are sorted alphabetically
/// - Null values are included (not skipped)
/// - Same input always produces identical output
pub fn to_json_sorted_value<T: serde::Serialize>(value: &T) -> serde_json::Result<String> {
    let json_value = serde_json::to_value(value)?;
    sort_json_value_to_string(&json_value)
}

/// Sort keys of a JSON value and return as a new Value
pub fn sort_json_value(value: &serde_json::Value) -> serde_json::Result<serde_json::Value> {
    let sorted_json = sort_json_value_to_string(value)?;
    serde_json::from_str(&sorted_json)
}

/// Recursively sort JSON keys for deterministic output
fn sort_json_value_to_string(value: &serde_json::Value) -> serde_json::Result<String> {
    match value {
        serde_json::Value::Object(map) => {
            let mut sorted_keys: Vec<String> = map.keys().cloned().collect();
            sorted_keys.sort_unstable();
            let mut new_map = serde_json::Map::new();
            for key in sorted_keys {
                let sorted_value = if let Some(val) = map.get(&key) {
                    if val.is_object() {
                        sort_json_value(val)?
                    } else if val.is_array() {
                        sort_array_keys(val)?
                    } else {
                        val.clone()
                    }
                } else {
                    serde_json::Value::Null
                };
                new_map.insert(key, sorted_value);
            }
            Ok(serde_json::to_string(&serde_json::Value::Object(new_map))?)
        }
        _ => Ok(serde_json::to_string(value)?),
    }
}

/// Sort keys within arrays of objects
fn sort_array_keys(value: &serde_json::Value) -> serde_json::Result<serde_json::Value> {
    match value {
        serde_json::Value::Array(arr) => {
            let sorted_arr: Result<Vec<serde_json::Value>, serde_json::Error> = arr
                .iter()
                .map(|v| {
                    if v.is_object() {
                        sort_json_value(v)
                    } else if v.is_array() {
                        sort_array_keys(v)
                    } else {
                        Ok(v.clone())
                    }
                })
                .collect();
            Ok(serde_json::Value::Array(sorted_arr?))
        }
        _ => Ok(value.clone()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_normalize_level() {
        assert_eq!(normalize_level("ERROR"), "error");
        assert_eq!(normalize_level("Info"), "info");
        assert_eq!(normalize_level("warn"), "warn");
        assert_eq!(normalize_level("DEBUG"), "debug");
    }

    #[test]
    fn test_normalize_source() {
        assert_eq!(normalize_source("Frontend"), "frontend");
        assert_eq!(normalize_source("BACKEND"), "backend");
        assert_eq!(normalize_source("Telemetry"), "telemetry");
    }

    #[test]
    fn test_normalize_timestamp_accepted_forms() {
        // RFC3339 with timezone → converted to UTC canonical
        assert_eq!(
            normalize_timestamp("2026-07-15T08:00:00.013Z").as_deref(),
            Some("2026-07-15T08:00:00.013Z")
        );
        assert_eq!(
            normalize_timestamp("2026-07-15T10:00:00.000+02:00").as_deref(),
            Some("2026-07-15T08:00:00.000Z")
        );
        // Naive dotted-millis and whole-second forms (assumed UTC)
        assert_eq!(
            normalize_timestamp("2026-07-15 08:00:00.013").as_deref(),
            Some("2026-07-15T08:00:00.013Z")
        );
        assert_eq!(
            normalize_timestamp("2026-07-15 08:00:00").as_deref(),
            Some("2026-07-15T08:00:00.000Z")
        );
        // Unparseable → None (caller keeps verbatim)
        assert_eq!(normalize_timestamp("not-a-timestamp"), None);
    }

    #[test]
    fn test_normalize_timestamp_python_comma_millis() {
        // Python's logging default: `2026-07-15 08:00:00,013`. The comma before
        // millis must normalize to canonical dotted-Z, not be kept verbatim.
        assert_eq!(
            normalize_timestamp("2026-07-15 08:00:00,013").as_deref(),
            Some("2026-07-15T08:00:00.013Z")
        );
        // Also the `T`-separated comma form, for completeness.
        assert_eq!(
            normalize_timestamp("2026-07-15T08:00:00,500").as_deref(),
            Some("2026-07-15T08:00:00.500Z")
        );
    }

    #[test]
    fn test_parse_timestamp_millis_mirrors_normalize() {
        // Known epoch: 2021-01-01T00:00:00Z = 1_609_459_200_000 ms
        assert_eq!(
            parse_timestamp_millis("2021-01-01T00:00:00.000Z"),
            Some(1_609_459_200_000)
        );
        // Comma-millis (Python) must parse identically to the dotted form so
        // those docs aren't dropped from numeric range filters.
        assert_eq!(
            parse_timestamp_millis("2021-01-01 00:00:00,250"),
            parse_timestamp_millis("2021-01-01T00:00:00.250Z")
        );
        assert_eq!(
            parse_timestamp_millis("2021-01-01 00:00:00,250"),
            Some(1_609_459_200_250)
        );
        // Unparseable → None (doc simply gets no timestamp_ms).
        assert_eq!(parse_timestamp_millis("not-a-timestamp"), None);
    }

    #[test]
    fn test_python_comma_millis_survives_from_raw() {
        // End-to-end: a Python-style comma-millis timestamp must be normalized
        // by from_raw so its stored timestamp is canonical and parseable to ms.
        let raw = RawLogEntry {
            timestamp: Some("2026-07-15 08:00:00,013".to_string()),
            level: Some("info".to_string()),
            message: Some("py".to_string()),
            source: Some("backend".to_string()),
            ..Default::default()
        };
        let entry = LogEntry::from_raw(raw, "backend");
        assert_eq!(entry.timestamp, "2026-07-15T08:00:00.013Z");
        assert!(
            parse_timestamp_millis(&entry.timestamp).is_some(),
            "normalized Python timestamp must parse to millis"
        );
    }

    #[test]
    fn test_generate_log_id_deterministic() {
        let id1 = generate_log_id(
            "2024-01-15T10:30:00.123Z",
            "error",
            "backend",
            "Failed to connect",
            Some("req-123"),
            None,
        );
        let id2 = generate_log_id(
            "2024-01-15T10:30:00.123Z",
            "error",
            "backend",
            "Failed to connect",
            Some("req-123"),
            None,
        );
        assert_eq!(id1, id2, "Same input should produce same log_id");
        assert_eq!(id1.len(), 16, "log_id should be 16 characters");
    }

    #[test]
    fn test_generate_log_id_different_for_different_input() {
        let id1 = generate_log_id(
            "2024-01-15T10:30:00.123Z",
            "error",
            "backend",
            "Failed to connect",
            Some("req-123"),
            None,
        );
        let id2 = generate_log_id(
            "2024-01-15T10:30:00.123Z",
            "info", // Different level
            "backend",
            "Failed to connect",
            Some("req-123"),
            None,
        );
        assert_ne!(id1, id2, "Different input should produce different log_id");
    }

    #[test]
    fn test_log_entry_from_raw() {
        let raw = RawLogEntry {
            timestamp: Some("2024-01-15T10:30:00.123Z".to_string()),
            level: Some("ERROR".to_string()),
            message: Some("Failed to connect".to_string()),
            source: None,
            request_id: Some("req-123".to_string()),
            user_id: None,
            route: Some("/api/auth".to_string()),
            method: Some("POST".to_string()),
            status_code: Some(500),
            duration_ms: Some(1234.5),
            error: Some(ErrorInfo {
                name: Some("ConnectionError".to_string()),
                message: Some("Timeout".to_string()),
                code: None,
            }),
            source_file: None,
            line_number: None,
            extra: Default::default(),
            parse_format: None,
            ingest_position: None,
        };

        let entry = LogEntry::from_raw(raw, "backend");

        assert_eq!(entry.level, "error"); // Normalized to lowercase
        assert_eq!(entry.source, "backend"); // Uses default
        assert_eq!(entry.message, "Failed to connect");
        assert_eq!(entry.request_id, Some("req-123".to_string()));
        assert_eq!(entry.status_code, Some(500));
        assert_eq!(entry.log_id.len(), 16);
    }

    #[test]
    fn test_schema_creation() {
        let schema = create_schema();
        let fields = LogFields::new(&schema).unwrap();

        // Verify all fields exist
        let _ = schema.get_field_name(fields.log_id);
        let _ = schema.get_field_name(fields.timestamp);
        let _ = schema.get_field_name(fields.level);
        let _ = schema.get_field_name(fields.source);
        let _ = schema.get_field_name(fields.message);
    }

    #[test]
    fn test_deterministic_json_output() {
        let entry = LogEntry {
            log_id: "a1b2c3d4e5f67890".to_string(),
            timestamp: "2024-01-15T10:30:00.123Z".to_string(),
            level: "error".to_string(),
            source: "backend".to_string(),
            message: "Failed to connect".to_string(),
            request_id: Some("req-123".to_string()),
            user_id: Some("user-456".to_string()),
            route: Some("/api/auth".to_string()),
            method: Some("POST".to_string()),
            status_code: Some(500),
            duration_ms: Some(1234.5),
            error: None,
            source_file: None,
            line_number: None,
            attributes: None,
            parse_format: "raw".to_string(),
        };

        let json1 = entry.to_json_sorted().unwrap();
        let json2 = entry.to_json_sorted().unwrap();

        assert_eq!(json1, json2, "Same entry should produce identical JSON");
    }

    // T10: Determinism Tests - Same log entry → identical JSON (key order, timestamp)
    #[test]
    fn test_deterministic_json_key_order() {
        // PRD requirement: Keys MUST be sorted alphabetically
        // Expected order: attributes, duration_ms, error, level, line_number, log_id,
        //                  message, method, parse_format, request_id, route, source,
        //                  source_file, status_code, timestamp, user_id
        let entry = LogEntry {
            log_id: "a1b2c3d4e5f67890".to_string(),
            timestamp: "2024-01-15T10:30:00.123Z".to_string(),
            level: "error".to_string(),
            source: "backend".to_string(),
            message: "Failed to connect".to_string(),
            request_id: Some("req-123".to_string()),
            user_id: Some("user-456".to_string()),
            route: Some("/api/auth".to_string()),
            method: Some("POST".to_string()),
            status_code: Some(500),
            duration_ms: Some(1234.5),
            error: None,
            source_file: None,
            line_number: None,
            attributes: None,
            parse_format: "raw".to_string(),
        };

        let json1 = entry.to_json_sorted().unwrap();
        let json2 = entry.to_json_sorted().unwrap();

        // Verify identical output
        assert_eq!(json1, json2, "Same entry should produce identical JSON");

        // Parse and verify keys are in sorted order
        let value: serde_json::Value = serde_json::from_str(&json1).unwrap();
        if let serde_json::Value::Object(map) = value {
            let keys: Vec<&str> = map.keys().map(|k| k.as_str()).collect();
            let mut sorted_keys = keys.clone();
            sorted_keys.sort();
            assert_eq!(keys, sorted_keys, "JSON keys must be sorted alphabetically");
        } else {
            panic!("Expected JSON object");
        }

        // Verify against PRD expected output (with nulls for None values)
        let expected = serde_json::json!({
            "attributes": null,
            "duration_ms": 1234.5,
            "error": null,
            "level": "error",
            "line_number": null,
            "log_id": "a1b2c3d4e5f67890",
            "message": "Failed to connect",
            "method": "POST",
            "parse_format": "raw",
            "request_id": "req-123",
            "route": "/api/auth",
            "source": "backend",
            "source_file": null,
            "status_code": 500,
            "timestamp": "2024-01-15T10:30:00.123Z",
            "user_id": "user-456"
        });
        let parsed: serde_json::Value = serde_json::from_str(&json1).unwrap();
        assert_eq!(parsed, expected, "JSON output must match PRD specification");
    }

    // T10: Timestamp normalization tests
    #[test]
    fn test_timestamp_normalization_iso8601() {
        // Verify timestamp is always UTC ISO8601 with milliseconds
        let raw = RawLogEntry {
            timestamp: Some("2024-01-15T10:30:00.123Z".to_string()),
            level: Some("info".to_string()),
            message: Some("Test message".to_string()),
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

        let entry = LogEntry::from_raw(raw, "backend");

        // Verify ISO8601 format with milliseconds
        assert_eq!(entry.timestamp, "2024-01-15T10:30:00.123Z");

        // Verify format matches ISO8601 with millis pattern
        let iso8601_with_millis =
            regex::Regex::new(r"^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}\.\d{3}Z$").unwrap();
        assert!(
            iso8601_with_millis.is_match(&entry.timestamp),
            "Timestamp must be ISO8601 with millis"
        );
    }

    #[test]
    fn test_timestamp_normalization_default() {
        // When timestamp is missing, it should generate valid ISO8601
        let raw = RawLogEntry {
            timestamp: None,
            level: Some("info".to_string()),
            message: Some("Test message".to_string()),
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

        let entry = LogEntry::from_raw(raw, "backend");

        // Verify ISO8601 format with milliseconds
        let iso8601_with_millis =
            regex::Regex::new(r"^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}\.\d{3}Z$").unwrap();
        assert!(
            iso8601_with_millis.is_match(&entry.timestamp),
            "Default timestamp must be ISO8601 with millis"
        );
    }

    // T10: Output hash comparison tests
    #[test]
    fn test_log_id_hash_deterministic() {
        // Same input fields should always produce same hash
        let timestamp = "2024-01-15T10:30:00.123Z";
        let level = "error";
        let source = "backend";
        let message = "Failed to connect";
        let request_id = Some("req-123");

        let id1 = generate_log_id(timestamp, level, source, message, request_id, None);
        let id2 = generate_log_id(timestamp, level, source, message, request_id, None);
        let id3 = generate_log_id(timestamp, level, source, message, request_id, None);

        assert_eq!(id1, id2, "Hash must be deterministic across calls");
        assert_eq!(id2, id3, "Hash must be deterministic across calls");
        assert_eq!(id1.len(), 16, "Hash must be 16 characters (8 bytes hex)");

        // Verify hex format
        assert!(
            id1.chars().all(|c| c.is_ascii_hexdigit()),
            "Hash must be hexadecimal"
        );
    }

    #[test]
    fn test_log_id_hash_different_inputs() {
        // Different inputs should produce different hashes
        let base = (
            "2024-01-15T10:30:00.123Z",
            "error",
            "backend",
            "Failed to connect",
            Some("req-123"),
        );

        // Different timestamp
        let id1 = generate_log_id(base.0, base.1, base.2, base.3, base.4, None);
        let id2 = generate_log_id(
            "2024-01-15T10:30:00.124Z",
            base.1,
            base.2,
            base.3,
            base.4,
            None,
        );
        assert_ne!(
            id1, id2,
            "Different timestamp should produce different hash"
        );

        // Different level
        let id3 = generate_log_id(base.0, "info", base.2, base.3, base.4, None);
        assert_ne!(id1, id3, "Different level should produce different hash");

        // Different source
        let id4 = generate_log_id(base.0, base.1, "frontend", base.3, base.4, None);
        assert_ne!(id1, id4, "Different source should produce different hash");

        // Different message
        let id5 = generate_log_id(
            base.0,
            base.1,
            base.2,
            "Failed to authenticate",
            base.4,
            None,
        );
        assert_ne!(id1, id5, "Different message should produce different hash");

        // Different request_id
        let id6 = generate_log_id(base.0, base.1, base.2, base.3, Some("req-456"), None);
        assert_ne!(
            id1, id6,
            "Different request_id should produce different hash"
        );

        // No request_id vs Some request_id
        let id7 = generate_log_id(base.0, base.1, base.2, base.3, None, None);
        assert_ne!(id1, id7, "Missing request_id should produce different hash");
    }

    #[test]
    fn test_deterministic_entry_serialization() {
        // PRD requirement: Same log entry → identical JSON output
        // when serialized multiple times
        let entry = LogEntry {
            log_id: "a1b2c3d4e5f67890".to_string(),
            timestamp: "2024-01-15T10:30:00.123Z".to_string(),
            level: "error".to_string(),
            source: "backend".to_string(),
            message: "Failed to connect".to_string(),
            request_id: Some("req-123".to_string()),
            user_id: Some("user-456".to_string()),
            route: Some("/api/auth".to_string()),
            method: Some("POST".to_string()),
            status_code: Some(500),
            duration_ms: Some(1234.5),
            error: None,
            source_file: None,
            line_number: None,
            attributes: None,
            parse_format: "raw".to_string(),
        };

        let outputs: Vec<String> = (0..10).map(|_| entry.to_json_sorted().unwrap()).collect();

        // All outputs should be identical
        for output in &outputs[1..] {
            assert_eq!(outputs[0], *output, "All serializations must be identical");
        }

        // Hash of output should be consistent
        let hashes: Vec<String> = outputs
            .iter()
            .map(|s| {
                let mut hasher = Sha256::new();
                hasher.update(s.as_bytes());
                hex::encode(hasher.finalize())
            })
            .collect();

        assert!(
            hashes.windows(2).all(|w| w[0] == w[1]),
            "All output hashes must be identical"
        );
    }

    #[test]
    fn test_deterministic_from_raw() {
        // Same RawLogEntry should produce same LogEntry
        let raw = RawLogEntry {
            timestamp: Some("2024-01-15T10:30:00.123Z".to_string()),
            level: Some("ERROR".to_string()),
            message: Some("Failed to connect".to_string()),
            source: Some("Backend".to_string()),
            request_id: Some("req-123".to_string()),
            user_id: Some("user-456".to_string()),
            route: Some("/api/auth".to_string()),
            method: Some("POST".to_string()),
            status_code: Some(500),
            duration_ms: Some(1234.5),
            error: None,
            source_file: None,
            line_number: None,
            extra: Default::default(),
            parse_format: None,
            ingest_position: None,
        };

        let entry1 = LogEntry::from_raw(raw.clone(), "backend");
        let entry2 = LogEntry::from_raw(raw.clone(), "backend");
        let entry3 = LogEntry::from_raw(raw, "backend");

        assert_eq!(
            entry1, entry2,
            "Same raw entry should produce identical LogEntry"
        );
        assert_eq!(
            entry2, entry3,
            "Same raw entry should produce identical LogEntry"
        );

        // Verify normalization is applied consistently
        assert_eq!(
            entry1.level, "error",
            "Level should be normalized to lowercase"
        );
        assert_eq!(
            entry1.source, "backend",
            "Source should be normalized to lowercase"
        );

        // Verify log_id is deterministic
        assert_eq!(entry1.log_id, entry2.log_id, "log_id must be deterministic");
        assert_eq!(entry2.log_id, entry3.log_id, "log_id must be deterministic");

        // Verify JSON output is identical
        let json1 = entry1.to_json_sorted().unwrap();
        let json2 = entry2.to_json_sorted().unwrap();
        assert_eq!(json1, json2, "JSON output must be identical");
    }

    // T10: Null value serialization test
    #[test]
    fn test_null_values_serialized() {
        // PRD requirement: null values must be serialized
        let entry = LogEntry {
            log_id: "a1b2c3d4e5f67890".to_string(),
            timestamp: "2024-01-15T10:30:00.123Z".to_string(),
            level: "error".to_string(),
            source: "backend".to_string(),
            message: "Failed to connect".to_string(),
            request_id: None,
            user_id: None,
            route: None,
            method: None,
            status_code: None,
            duration_ms: None,
            error: None,
            source_file: None,
            line_number: None,
            attributes: None,
            parse_format: "raw".to_string(),
        };

        let json = entry.to_json_sorted().unwrap();

        // Verify null fields are present in output
        assert!(
            json.contains("\"error\":null"),
            "error field should be null"
        );
        assert!(
            json.contains("\"request_id\":null"),
            "request_id field should be null"
        );
        assert!(
            json.contains("\"user_id\":null"),
            "user_id field should be null"
        );
        assert!(
            json.contains("\"duration_ms\":null"),
            "duration_ms field should be null"
        );

        // Parse and count null values
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        if let serde_json::Value::Object(map) = value {
            let null_count = map.values().filter(|v| v.is_null()).count();
            assert_eq!(
                null_count, 10,
                "Should have 10 null fields for missing optional data"
            );
        } else {
            panic!("Expected JSON object");
        }
    }

    #[cfg(test)]
    mod proptests {
        use super::*;
        use proptest::prelude::*;

        proptest! {
            #[test]
            fn proptest_log_id_determinism(
                timestamp in ".*",
                level in ".*",
                source in ".*",
                message in ".*",
                request_id in prop::option::of(".*")
            ) {
                let id1 = generate_log_id(&timestamp, &level, &source, &message, request_id.as_deref(), None);
                let id2 = generate_log_id(&timestamp, &level, &source, &message, request_id.as_deref(), None);
                assert_eq!(id1, id2);
                assert_eq!(id1.len(), 16);
            }

            #[test]
            fn proptest_json_sorted_keys_valid(
                log_id in ".*",
                timestamp in ".*",
                level in ".*",
                source in ".*",
                message in ".*",
                request_id in prop::option::of(".*"),
                user_id in prop::option::of(".*"),
                route in prop::option::of(".*"),
                method in prop::option::of(".*"),
                status_code in prop::option::of(0u16..65535u16),
                duration_ms in prop::option::of(0f64..1000000f64),
                error_name in prop::option::of(".*"),
                error_message in prop::option::of(".*"),
                error_code in prop::option::of(".*"),
                source_file in prop::option::of(".*"),
                line_number in prop::option::of(0usize..10000usize),
            ) {
                let error = if error_name.is_some() || error_message.is_some() || error_code.is_some() {
                    Some(ErrorInfo {
                        name: error_name,
                        message: error_message,
                        code: error_code,
                    })
                } else {
                    None
                };

                let entry = LogEntry {
                    log_id,
                    timestamp,
                    level,
                    source,
                    message,
                    request_id,
                    user_id,
                    route,
                    method,
                    status_code,
                    duration_ms,
                    error,
                    source_file,
                    line_number,
                    attributes: None,
            parse_format: "raw".to_string(),
                };

                let json = entry.to_json_sorted().unwrap();
                let value: serde_json::Value = serde_json::from_str(&json).unwrap();

                if let serde_json::Value::Object(map) = value {
                    let keys: Vec<&str> = map.keys().map(|k| k.as_str()).collect();
                    let mut sorted_keys = keys.clone();
                    sorted_keys.sort();
                    assert_eq!(keys, sorted_keys, "Keys must be sorted alphabetically");

                    // Verify all fields are present (even if null)
                    assert_eq!(keys.len(), 16, "All 16 fields must be present in JSON output");
                } else {
                    panic!("Expected JSON object");
                }
            }
        }
    }
}
