//! Custom Tantivy collectors for streaming aggregation.
//!
//! Instead of loading all matching documents into memory as `LogEntry` objects
//! and then aggregating, these collectors aggregate directly during the
//! Tantivy search phase — reading only the fields needed for each aggregation.
//!
//! ## Why custom collectors?
//!
//! The previous approach had three problems:
//! 1. `search()` clamped results to 10K — incomplete aggregation for large indexes
//! 2. Every doc was fully deserialized into a `LogEntry` (16 fields, ~1KB each)
//! 3. All docs lived in a `Vec<LogEntry>` simultaneously — O(n) memory
//!
//! Custom collectors solve all three: O(k) memory (k = unique groups),
//! no 10K limit, and per-doc overhead is just reading the fields we need.

use std::collections::{HashMap, HashSet};

use sha2::{Digest, Sha256};
use tantivy::collector::{Collector, SegmentCollector};
use tantivy::columnar::StrColumn;
use tantivy::schema::{Field, OwnedValue};
use tantivy::store::StoreReader;
use tantivy::{DocId, Score, SegmentReader};

// ---------------------------------------------------------------------------
// FieldCountCollector — count docs by a single field's values
// ---------------------------------------------------------------------------

/// Counts matching documents by a single string field's values.
///
/// Reads values from the columnar **fast field**, not the document store: during
/// collection we tally term ordinals (`u64`) with no per-doc string allocation,
/// and resolve ordinals to strings once at harvest. This is O(1) column lookups
/// per doc instead of decompressing and deserializing the whole stored document
/// (all 16 fields) just to read one — the difference between ~15ms and ~625ms
/// over a million docs.
///
/// Docs with no value for the field are counted under `default_value` (e.g.
/// "unknown" for route, "anonymous" for user_id). **Requires the field to be
/// declared `FAST` in the schema** — otherwise the column is absent and every
/// doc falls through to `default_value`.
///
/// Returns a `HashMap<String, usize>` mapping field values to counts.
pub struct FieldCountCollector {
    field_name: String,
    default_value: String,
}

impl FieldCountCollector {
    pub fn new(field_name: &str, default_value: &str) -> Self {
        Self {
            field_name: field_name.to_string(),
            default_value: default_value.to_string(),
        }
    }
}

/// Per-segment state for `FieldCountCollector`.
///
/// Counts are keyed by per-segment term ordinal during collection; the ordinals
/// are resolved to their string values (via the segment's dictionary) at harvest,
/// so cross-segment merging happens on strings and stays correct.
pub struct FieldCountSegmentCollector {
    column: Option<StrColumn>,
    default_value: String,
    /// Counts indexed by term ordinal. Ordinals are dense (`0..num_terms`), so a
    /// flat `Vec` avoids hashing on the per-doc hot path entirely.
    ord_counts: Vec<usize>,
    missing: usize,
}

impl Collector for FieldCountCollector {
    type Fruit = HashMap<String, usize>;
    type Child = FieldCountSegmentCollector;

    fn for_segment(
        &self,
        _segment_id: u32,
        reader: &SegmentReader,
    ) -> tantivy::Result<Self::Child> {
        let column = reader.fast_fields().str(&self.field_name)?;
        let num_terms = column.as_ref().map(|c| c.num_terms()).unwrap_or(0);
        Ok(FieldCountSegmentCollector {
            column,
            default_value: self.default_value.clone(),
            ord_counts: vec![0usize; num_terms],
            missing: 0,
        })
    }

    fn requires_scoring(&self) -> bool {
        false
    }

    fn merge_fruits(
        &self,
        child_fruits: Vec<HashMap<String, usize>>,
    ) -> tantivy::Result<HashMap<String, usize>> {
        let mut merged = HashMap::new();
        for fruit in child_fruits {
            for (key, count) in fruit {
                *merged.entry(key).or_insert(0) += count;
            }
        }
        Ok(merged)
    }
}

impl SegmentCollector for FieldCountSegmentCollector {
    type Fruit = HashMap<String, usize>;

    fn collect(&mut self, doc: DocId, _score: Score) {
        match &self.column {
            Some(col) => {
                let mut found = false;
                for ord in col.term_ords(doc) {
                    self.ord_counts[ord as usize] += 1;
                    found = true;
                }
                if !found {
                    self.missing += 1;
                }
            }
            None => self.missing += 1,
        }
    }

    fn harvest(self) -> Self::Fruit {
        let mut counts: HashMap<String, usize> = HashMap::new();
        if let Some(col) = &self.column {
            let mut buf = String::new();
            for (ord, &count) in self.ord_counts.iter().enumerate() {
                if count == 0 {
                    continue;
                }
                buf.clear();
                if col.ord_to_str(ord as u64, &mut buf).unwrap_or(false) {
                    counts.insert(buf.clone(), count);
                }
            }
        }
        if self.missing > 0 {
            *counts.entry(self.default_value.clone()).or_insert(0) += self.missing;
        }
        counts
    }
}

// ---------------------------------------------------------------------------
// ErrorAggCollector — group errors by fingerprint during search
// ---------------------------------------------------------------------------

/// An error group built during streaming aggregation.
///
/// Unlike the command-layer `ErrorGroup` (which uses `Vec<String>` for
/// `sample_log_ids` and `usize` for `affected_users`), this intermediate
/// representation uses `HashSet<String>` for efficient dedup during collection.
#[derive(Debug)]
pub struct AggregatedErrorGroup {
    /// Fingerprint hash (error name + message)
    pub fingerprint: String,
    /// Error name/type
    pub error_name: Option<String>,
    /// Error message
    pub error_message: Option<String>,
    /// Masked message template shared by the group (variable parts →
    /// <num>/<uuid>/<hex>/<ip>/<str> placeholders)
    pub template: String,
    /// Total occurrence count
    pub count: usize,
    /// Unique affected user IDs
    pub affected_users: HashSet<String>,
    /// First occurrence timestamp
    pub first_seen: String,
    /// Most recent occurrence timestamp
    pub last_seen: String,
    /// Sample log IDs (max 3)
    pub sample_log_ids: Vec<String>,
}

/// Collector that groups error documents by fingerprint during the search phase.
///
/// For each matching document, reads `error_name`, `error_message`, `user_id`,
/// `timestamp`, `log_id`, and `message` from the stored document. Groups by
/// fingerprint (SHA-256 of error_name + error_message) and tracks:
/// - Count per group
/// - Unique affected users (via HashSet)
/// - First/last seen timestamps
/// - Up to 3 sample log IDs per group
pub struct ErrorAggCollector {
    error_name_field: Field,
    error_message_field: Field,
    user_id_field: Field,
    timestamp_field: Field,
    log_id_field: Field,
    message_field: Field,
}

impl ErrorAggCollector {
    pub fn new(
        error_name_field: Field,
        error_message_field: Field,
        user_id_field: Field,
        timestamp_field: Field,
        log_id_field: Field,
        message_field: Field,
    ) -> Self {
        Self {
            error_name_field,
            error_message_field,
            user_id_field,
            timestamp_field,
            log_id_field,
            message_field,
        }
    }
}

/// Per-segment state for `ErrorAggCollector`.
pub struct ErrorAggSegmentCollector {
    error_name_field: Field,
    error_message_field: Field,
    user_id_field: Field,
    timestamp_field: Field,
    log_id_field: Field,
    message_field: Field,
    store_reader: StoreReader,
    groups: HashMap<String, AggregatedErrorGroup>,
}

impl Collector for ErrorAggCollector {
    type Fruit = HashMap<String, AggregatedErrorGroup>;
    type Child = ErrorAggSegmentCollector;

    fn for_segment(
        &self,
        _segment_id: u32,
        reader: &SegmentReader,
    ) -> tantivy::Result<Self::Child> {
        Ok(ErrorAggSegmentCollector {
            error_name_field: self.error_name_field,
            error_message_field: self.error_message_field,
            user_id_field: self.user_id_field,
            timestamp_field: self.timestamp_field,
            log_id_field: self.log_id_field,
            message_field: self.message_field,
            store_reader: reader.get_store_reader(1)?,
            groups: HashMap::new(),
        })
    }

    fn requires_scoring(&self) -> bool {
        false
    }

    fn merge_fruits(
        &self,
        child_fruits: Vec<HashMap<String, AggregatedErrorGroup>>,
    ) -> tantivy::Result<HashMap<String, AggregatedErrorGroup>> {
        let mut merged: HashMap<String, AggregatedErrorGroup> = HashMap::new();
        for fruit in child_fruits {
            for (key, group) in fruit {
                match merged.entry(key) {
                    std::collections::hash_map::Entry::Occupied(mut entry) => {
                        let existing = entry.get_mut();
                        existing.count += group.count;
                        existing.affected_users.extend(group.affected_users);
                        if group.first_seen < existing.first_seen {
                            existing.first_seen = group.first_seen;
                        }
                        if group.last_seen > existing.last_seen {
                            existing.last_seen = group.last_seen;
                        }
                        if existing.sample_log_ids.len() < 3 {
                            let needed = 3 - existing.sample_log_ids.len();
                            existing
                                .sample_log_ids
                                .extend(group.sample_log_ids.into_iter().take(needed));
                        }
                    }
                    std::collections::hash_map::Entry::Vacant(entry) => {
                        entry.insert(group);
                    }
                }
            }
        }
        Ok(merged)
    }
}

impl SegmentCollector for ErrorAggSegmentCollector {
    type Fruit = HashMap<String, AggregatedErrorGroup>;

    fn collect(&mut self, doc: DocId, _score: Score) {
        let tantivy_doc = match self.store_reader.get(doc) {
            Ok(d) => d,
            Err(_) => return,
        };

        // Read only the 6 fields we need (out of 16 total)
        let error_name = get_text_field(&tantivy_doc, self.error_name_field);
        let error_message = get_text_field(&tantivy_doc, self.error_message_field);
        let user_id = get_text_field(&tantivy_doc, self.user_id_field);
        let log_id = get_text_field(&tantivy_doc, self.log_id_field).unwrap_or_default();
        let message = get_text_field(&tantivy_doc, self.message_field).unwrap_or_default();
        let timestamp = get_text_field(&tantivy_doc, self.timestamp_field).unwrap_or_default();

        // Group by (error_name, masked template): near-identical errors
        // differing only in UUIDs/hosts/durations collapse into one group.
        let (fingerprint, template, err_name, err_msg) =
            if error_name.is_some() || error_message.is_some() {
                let name = error_name.as_deref().unwrap_or("Unknown");
                let msg = error_message.as_deref().unwrap_or("");
                let template = crate::cmd::errors::normalize_template(msg);
                (
                    generate_fingerprint(name, &template),
                    template,
                    error_name,
                    error_message,
                )
            } else {
                // Fallback: use the log message for fingerprinting
                let template = crate::cmd::errors::normalize_template(&message);
                (
                    generate_fingerprint("Unknown", &template),
                    template,
                    None,
                    Some(message),
                )
            };

        // Update or create group
        let group =
            self.groups
                .entry(fingerprint.clone())
                .or_insert_with(|| AggregatedErrorGroup {
                    fingerprint,
                    error_name: err_name.clone(),
                    error_message: err_msg.clone(),
                    template,
                    count: 0,
                    affected_users: HashSet::new(),
                    first_seen: timestamp.clone(),
                    last_seen: timestamp.clone(),
                    sample_log_ids: Vec::new(),
                });

        group.count += 1;

        if let Some(uid) = user_id {
            group.affected_users.insert(uid);
        }

        if timestamp < group.first_seen {
            group.first_seen = timestamp;
        } else if timestamp > group.last_seen {
            group.last_seen = timestamp;
        }

        if group.sample_log_ids.len() < 3 {
            group.sample_log_ids.push(log_id);
        }
    }

    fn harvest(self) -> Self::Fruit {
        self.groups
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Extract the first text value from a Tantivy document for a given field.
fn get_text_field(doc: &tantivy::TantivyDocument, field: Field) -> Option<String> {
    doc.get_first(field).and_then(|v: &OwnedValue| match v {
        OwnedValue::Str(s) => Some(s.clone()),
        OwnedValue::PreTokStr(s) => Some(s.text.clone()),
        _ => None,
    })
}

/// Generate a deterministic fingerprint from error name and message.
/// Same algorithm as `cmd::errors::generate_fingerprint`.
fn generate_fingerprint(name: &str, message: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(name.as_bytes());
    hasher.update(b"|");
    hasher.update(message.as_bytes());
    let result = hasher.finalize();
    hex::encode(&result[..8]) // First 8 bytes = 16 hex chars
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::{create_schema, LogFields};
    use tantivy::directory::RamDirectory;
    use tantivy::{doc, Index};

    fn setup_index_with_entries() -> (Index, LogFields) {
        let schema = create_schema();
        let fields = LogFields::new(&schema).unwrap();
        let dir = RamDirectory::create();
        let index = Index::open_or_create(dir, schema).unwrap();
        let mut writer = index.writer_with_num_threads(1, 15_000_000).unwrap();

        // Add 5 entries with different levels, routes, users
        #[allow(clippy::type_complexity)]
        let entries: Vec<(&str, &str, &str, &str, Option<&str>, Option<&str>)> = vec![
            (
                "id1",
                "info",
                "source1",
                "msg1",
                Some("user1"),
                Some("/api/test"),
            ),
            (
                "id2",
                "info",
                "source1",
                "msg2",
                Some("user2"),
                Some("/api/test"),
            ),
            (
                "id3",
                "error",
                "source1",
                "msg3",
                Some("user1"),
                Some("/api/data"),
            ),
            ("id4", "warn", "source2", "msg4", None, None),
            (
                "id5",
                "error",
                "source2",
                "msg5",
                Some("user3"),
                Some("/api/test"),
            ),
        ];

        for (log_id, level, source, message, user_id, route) in entries {
            let mut d = doc!(
                fields.log_id => log_id.to_string(),
                fields.timestamp => "2026-03-22T10:00:00.000Z".to_string(),
                fields.level => level.to_string(),
                fields.source => source.to_string(),
                fields.message => message.to_string(),
                fields.full_text => message.to_string(),
            );
            if let Some(r) = route {
                d.add_text(fields.route, r);
            }
            if let Some(u) = user_id {
                d.add_text(fields.user_id, u);
            }
            writer.add_document(d).unwrap();
        }
        writer.commit().unwrap();

        (index, fields)
    }

    #[test]
    fn test_field_count_by_level() {
        let (index, _fields) = setup_index_with_entries();
        let reader = index.reader().unwrap();
        let searcher = reader.searcher();

        let collector = FieldCountCollector::new("level", "");
        let counts = searcher
            .search(&tantivy::query::AllQuery, &collector)
            .unwrap();

        assert_eq!(counts.get("info"), Some(&2));
        assert_eq!(counts.get("error"), Some(&2));
        assert_eq!(counts.get("warn"), Some(&1));
    }

    #[test]
    fn test_field_count_by_route_with_default() {
        let (index, _fields) = setup_index_with_entries();
        let reader = index.reader().unwrap();
        let searcher = reader.searcher();

        let collector = FieldCountCollector::new("route", "unknown");
        let counts = searcher
            .search(&tantivy::query::AllQuery, &collector)
            .unwrap();

        assert_eq!(counts.get("/api/test"), Some(&3));
        assert_eq!(counts.get("/api/data"), Some(&1));
        assert_eq!(counts.get("unknown"), Some(&1)); // id4 has no route
    }

    #[test]
    fn test_field_count_by_user_with_default() {
        let (index, _fields) = setup_index_with_entries();
        let reader = index.reader().unwrap();
        let searcher = reader.searcher();

        let collector = FieldCountCollector::new("user_id", "anonymous");
        let counts = searcher
            .search(&tantivy::query::AllQuery, &collector)
            .unwrap();

        assert_eq!(counts.get("user1"), Some(&2));
        assert_eq!(counts.get("user2"), Some(&1));
        assert_eq!(counts.get("user3"), Some(&1));
        assert_eq!(counts.get("anonymous"), Some(&1)); // id4 has no user
    }

    #[test]
    fn test_error_agg_collector() {
        let schema = create_schema();
        let fields = LogFields::new(&schema).unwrap();
        let dir = RamDirectory::create();
        let index = Index::open_or_create(dir, schema).unwrap();
        let mut writer = index.writer_with_num_threads(1, 15_000_000).unwrap();

        // Add error entries
        let errors = vec![
            (
                "e1",
                "DatabaseError",
                "Connection timeout",
                "user1",
                "2026-03-22T10:00:00.000Z",
            ),
            (
                "e2",
                "DatabaseError",
                "Connection timeout",
                "user2",
                "2026-03-22T10:01:00.000Z",
            ),
            (
                "e3",
                "AuthError",
                "Invalid token",
                "user1",
                "2026-03-22T10:02:00.000Z",
            ),
        ];

        for (log_id, err_name, err_msg, user_id, ts) in &errors {
            let d = doc!(
                fields.log_id => log_id.to_string(),
                fields.timestamp => ts.to_string(),
                fields.level => "error".to_string(),
                fields.source => "test".to_string(),
                fields.message => format!("Failed: {err_msg}"),
                fields.full_text => format!("Failed: {err_msg}"),
                fields.error_name => err_name.to_string(),
                fields.error_message => err_msg.to_string(),
                fields.user_id => user_id.to_string(),
            );
            writer.add_document(d).unwrap();
        }
        writer.commit().unwrap();

        let reader = index.reader().unwrap();
        let searcher = reader.searcher();

        let collector = ErrorAggCollector::new(
            fields.error_name,
            fields.error_message,
            fields.user_id,
            fields.timestamp,
            fields.log_id,
            fields.message,
        );

        let groups = searcher
            .search(&tantivy::query::AllQuery, &collector)
            .unwrap();

        assert_eq!(groups.len(), 2, "Should have 2 error groups");

        let db_group = groups
            .values()
            .find(|g| g.error_name.as_deref() == Some("DatabaseError"))
            .unwrap();
        assert_eq!(db_group.count, 2);
        assert_eq!(db_group.affected_users.len(), 2);
        assert_eq!(db_group.sample_log_ids.len(), 2);

        let auth_group = groups
            .values()
            .find(|g| g.error_name.as_deref() == Some("AuthError"))
            .unwrap();
        assert_eq!(auth_group.count, 1);
        assert_eq!(auth_group.affected_users.len(), 1);
    }

    #[test]
    fn test_error_agg_fallback_to_message() {
        let schema = create_schema();
        let fields = LogFields::new(&schema).unwrap();
        let dir = RamDirectory::create();
        let index = Index::open_or_create(dir, schema).unwrap();
        let mut writer = index.writer_with_num_threads(1, 15_000_000).unwrap();

        // Error with no structured error info — fingerprint uses message
        let d = doc!(
            fields.log_id => "e_no_err".to_string(),
            fields.timestamp => "2026-03-22T10:00:00.000Z".to_string(),
            fields.level => "error".to_string(),
            fields.source => "test".to_string(),
            fields.message => "Generic error occurred".to_string(),
            fields.full_text => "Generic error occurred".to_string(),
        );
        writer.add_document(d).unwrap();
        writer.commit().unwrap();

        let reader = index.reader().unwrap();
        let searcher = reader.searcher();

        let collector = ErrorAggCollector::new(
            fields.error_name,
            fields.error_message,
            fields.user_id,
            fields.timestamp,
            fields.log_id,
            fields.message,
        );

        let groups = searcher
            .search(&tantivy::query::AllQuery, &collector)
            .unwrap();

        assert_eq!(groups.len(), 1);
        let group = groups.values().next().unwrap();
        assert_eq!(group.count, 1);
        assert_eq!(group.error_name, None);
        assert_eq!(
            group.error_message,
            Some("Generic error occurred".to_string())
        );
    }

    #[test]
    fn test_generate_fingerprint_deterministic() {
        let fp1 = generate_fingerprint("TestError", "msg");
        let fp2 = generate_fingerprint("TestError", "msg");
        assert_eq!(fp1, fp2);
        assert_eq!(fp1.len(), 16);
    }

    #[test]
    fn test_generate_fingerprint_different() {
        let fp1 = generate_fingerprint("TestError", "msg1");
        let fp2 = generate_fingerprint("TestError", "msg2");
        assert_ne!(fp1, fp2);
    }

    #[test]
    fn test_empty_index_field_count() {
        let schema = create_schema();
        let _fields = LogFields::new(&schema).unwrap();
        let dir = RamDirectory::create();
        let index = Index::open_or_create(dir, schema).unwrap();

        let reader = index.reader().unwrap();
        let searcher = reader.searcher();

        let collector = FieldCountCollector::new("level", "");
        let counts = searcher
            .search(&tantivy::query::AllQuery, &collector)
            .unwrap();

        assert!(counts.is_empty());
    }

    #[test]
    fn test_empty_index_error_agg() {
        let schema = create_schema();
        let fields = LogFields::new(&schema).unwrap();
        let dir = RamDirectory::create();
        let index = Index::open_or_create(dir, schema).unwrap();

        let reader = index.reader().unwrap();
        let searcher = reader.searcher();

        let collector = ErrorAggCollector::new(
            fields.error_name,
            fields.error_message,
            fields.user_id,
            fields.timestamp,
            fields.log_id,
            fields.message,
        );

        let groups = searcher
            .search(&tantivy::query::AllQuery, &collector)
            .unwrap();

        assert!(groups.is_empty());
    }
}
