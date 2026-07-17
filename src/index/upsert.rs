//! Shared upsert logic for deduplicated log entry insertion.
//!
//! Single source of truth for building Tantivy documents from LogEntry and
//! upserting them by log_id. Used by ingest, serve (watch), and index rebuild.

use anyhow::{Context, Result};
use tantivy::{doc, Index, IndexReader, IndexWriter, Term};

use crate::schema::{LogEntry, LogFields};

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Upsert a log entry into the index.
///
/// Deletes any existing document with the same `log_id`, then adds the new one.
/// When `skip_dedup` is true (fast-path for empty indexes), only `add_document`
/// is called, avoiding the overhead of a no-op `delete_term`.
pub fn upsert_entry(
    writer: &mut IndexWriter,
    fields: &LogFields,
    entry: &LogEntry,
    skip_dedup: bool,
) -> Result<()> {
    if !skip_dedup {
        let delete_term = Term::from_field_text(fields.log_id, &entry.log_id);
        writer.delete_term(delete_term);
    }

    let document = build_document(fields, entry);
    writer
        .add_document(document)
        .context("Failed to add document to index")?;

    Ok(())
}

/// Count the total number of documents currently in the index.
///
/// Uses the searcher's `num_docs()` which is O(1) — it reads the segment
/// metadata, not the actual documents.
pub fn count_docs(reader: &IndexReader) -> u64 {
    reader.searcher().num_docs()
}

/// Create an [`IndexReader`] for the given index with manual reload policy.
///
/// Callers should invoke `reader.reload()` after committing to see new docs.
pub fn reader_for(index: &Index) -> Result<IndexReader> {
    use tantivy::ReloadPolicy;
    index
        .reader_builder()
        .reload_policy(ReloadPolicy::Manual)
        .try_into()
        .context("Failed to create index reader")
}

/// Detect whether the index is empty (no segments or zero docs).
/// When true, upsert can safely skip `delete_term` since there's nothing to delete.
pub fn index_is_empty(reader: &IndexReader) -> bool {
    reader.searcher().num_docs() == 0
}

// ---------------------------------------------------------------------------
// Document builder (private)
// ---------------------------------------------------------------------------

/// Build a Tantivy document from a LogEntry.
fn build_document(fields: &LogFields, entry: &LogEntry) -> tantivy::TantivyDocument {
    // Full-text content combining all searchable fields
    let mut full_text = format!(
        "{} {} {} {} {}",
        entry.message,
        entry.route.as_deref().unwrap_or(""),
        entry.user_id.as_deref().unwrap_or(""),
        entry.request_id.as_deref().unwrap_or(""),
        entry
            .error
            .as_ref()
            .map(|e| e.message.as_deref().unwrap_or(""))
            .unwrap_or("")
    );

    // Scalar attributes become "key value" tokens so `--query` reaches them.
    if let Some(ref attributes) = entry.attributes {
        for (key, value) in attributes {
            match value {
                serde_json::Value::String(s) => {
                    full_text.push(' ');
                    full_text.push_str(key);
                    full_text.push(' ');
                    full_text.push_str(s);
                }
                serde_json::Value::Number(n) => {
                    full_text.push(' ');
                    full_text.push_str(key);
                    full_text.push(' ');
                    full_text.push_str(&n.to_string());
                }
                serde_json::Value::Bool(b) => {
                    full_text.push(' ');
                    full_text.push_str(key);
                    full_text.push(' ');
                    full_text.push_str(if *b { "true" } else { "false" });
                }
                // Nested objects/arrays stay retrievable via the stored
                // attributes JSON but are not tokenized.
                _ => {}
            }
        }
    }

    let mut document = doc!(
        fields.full_text => full_text,
        fields.log_id => entry.log_id.clone(),
        fields.timestamp => entry.timestamp.clone(),
        fields.level => entry.level.clone(),
        fields.source => entry.source.clone(),
        fields.message => entry.message.clone(),
        fields.parse_format => entry.parse_format.clone(),
    );

    // Numeric mirror of the timestamp for fast range filtering. Skipped when the
    // timestamp can't be parsed (kept verbatim upstream) or predates the epoch;
    // such docs simply don't participate in time-range filters.
    if let Some(ms) = crate::schema::parse_timestamp_millis(&entry.timestamp) {
        if ms >= 0 {
            document.add_u64(fields.timestamp_ms, ms as u64);
        }
    }

    // Optional text fields
    if let Some(ref v) = entry.request_id {
        document.add_text(fields.request_id, v);
    }
    if let Some(ref v) = entry.user_id {
        document.add_text(fields.user_id, v);
    }
    if let Some(ref v) = entry.route {
        document.add_text(fields.route, v);
    }
    if let Some(ref v) = entry.method {
        document.add_text(fields.method, v);
    }

    // Optional numeric fields
    if let Some(v) = entry.status_code {
        document.add_u64(fields.status_code, v as u64);
    }
    if let Some(v) = entry.duration_ms {
        document.add_f64(fields.duration_ms, v);
    }

    // Error fields (flattened)
    if let Some(ref error) = entry.error {
        if let Some(ref name) = error.name {
            document.add_text(fields.error_name, name);
        }
        if let Some(ref msg) = error.message {
            document.add_text(fields.error_message, msg);
        }
    }

    // Source location fields
    if let Some(ref v) = entry.source_file {
        document.add_text(fields.source_file, v);
    }
    if let Some(v) = entry.line_number {
        document.add_u64(fields.line_number, v as u64);
    }

    // Attributes stored as canonical JSON for round-tripping
    if let Some(ref attributes) = entry.attributes {
        if let Ok(json) = serde_json::to_string(attributes) {
            document.add_text(fields.attributes, json);
        }
    }

    document
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::{create_schema, ErrorInfo, LogFields};
    use tantivy::directory::RamDirectory;

    fn make_entry(log_id: &str) -> LogEntry {
        LogEntry {
            log_id: log_id.to_string(),
            timestamp: "2026-03-22T10:00:00.000Z".to_string(),
            level: "info".to_string(),
            source: "test".to_string(),
            message: format!("message for {log_id}"),
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
        }
    }

    fn make_entry_full(log_id: &str) -> LogEntry {
        LogEntry {
            log_id: log_id.to_string(),
            timestamp: "2026-03-22T10:00:00.000Z".to_string(),
            level: "error".to_string(),
            source: "backend".to_string(),
            message: "Full entry".to_string(),
            request_id: Some("req-1".to_string()),
            user_id: Some("user-1".to_string()),
            route: Some("/api/test".to_string()),
            method: Some("GET".to_string()),
            status_code: Some(200),
            duration_ms: Some(42.5),
            error: Some(ErrorInfo {
                name: Some("TestError".to_string()),
                message: Some("boom".to_string()),
                code: Some("E001".to_string()),
            }),
            source_file: Some("src/main.rs".to_string()),
            line_number: Some(42),
            attributes: None,
            parse_format: "raw".to_string(),
        }
    }

    fn setup_index() -> (Index, LogFields, IndexWriter) {
        let schema = create_schema();
        let fields = LogFields::new(&schema).unwrap();
        let dir = RamDirectory::create();
        let index = Index::open_or_create(dir, schema).unwrap();
        let writer = index.writer_with_num_threads(1, 15_000_000).unwrap();
        (index, fields, writer)
    }

    #[test]
    fn test_upsert_inserts_new_doc() {
        let (index, fields, mut writer) = setup_index();
        let entry = make_entry("abc123");
        upsert_entry(&mut writer, &fields, &entry, false).unwrap();
        writer.commit().unwrap();

        let reader = index.reader().unwrap();
        assert_eq!(count_docs(&reader), 1);
    }

    #[test]
    fn test_upsert_deduplicates_same_log_id() {
        let (index, fields, mut writer) = setup_index();

        // Insert twice with same log_id
        let entry = make_entry("dup_id");
        upsert_entry(&mut writer, &fields, &entry, false).unwrap();
        upsert_entry(&mut writer, &fields, &entry, false).unwrap();
        writer.commit().unwrap();

        let reader = index.reader().unwrap();
        assert_eq!(
            count_docs(&reader),
            1,
            "Should have exactly 1 doc after upsert"
        );
    }

    #[test]
    fn test_upsert_different_log_ids_keeps_both() {
        let (index, fields, mut writer) = setup_index();

        upsert_entry(&mut writer, &fields, &make_entry("id1"), false).unwrap();
        upsert_entry(&mut writer, &fields, &make_entry("id2"), false).unwrap();
        writer.commit().unwrap();

        let reader = index.reader().unwrap();
        assert_eq!(count_docs(&reader), 2);
    }

    #[test]
    fn test_fast_path_skip_dedup_produces_same_result() {
        let (index, fields, mut writer) = setup_index();

        // skip_dedup = true (fast path for empty index)
        upsert_entry(&mut writer, &fields, &make_entry("fast1"), true).unwrap();
        upsert_entry(&mut writer, &fields, &make_entry("fast2"), true).unwrap();
        writer.commit().unwrap();

        let reader = index.reader().unwrap();
        assert_eq!(count_docs(&reader), 2);
    }

    #[test]
    fn test_fast_path_does_not_dedup() {
        let (index, fields, mut writer) = setup_index();

        // skip_dedup = true — deliberately insert duplicate
        let entry = make_entry("dup");
        upsert_entry(&mut writer, &fields, &entry, true).unwrap();
        upsert_entry(&mut writer, &fields, &entry, true).unwrap();
        writer.commit().unwrap();

        let reader = index.reader().unwrap();
        // Without dedup, we get 2 docs (caller's responsibility to not abuse fast-path)
        assert_eq!(count_docs(&reader), 2);
    }

    #[test]
    fn test_full_entry_roundtrip() {
        let (index, fields, mut writer) = setup_index();
        let entry = make_entry_full("full1");
        upsert_entry(&mut writer, &fields, &entry, false).unwrap();
        writer.commit().unwrap();

        let reader = index.reader().unwrap();
        assert_eq!(count_docs(&reader), 1);
    }

    #[test]
    fn test_index_is_empty_detection() {
        let (index, _fields, _writer) = setup_index();
        let reader = index.reader().unwrap();
        assert!(index_is_empty(&reader), "New index should be empty");
    }

    #[test]
    fn test_count_docs_matches_actual() {
        let (index, fields, mut writer) = setup_index();
        for i in 0..5 {
            upsert_entry(&mut writer, &fields, &make_entry(&format!("id{i}")), true).unwrap();
        }
        writer.commit().unwrap();

        let reader = index.reader().unwrap();
        reader.reload().unwrap();
        assert_eq!(count_docs(&reader), 5);
    }
}
