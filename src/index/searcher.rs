//! Search functionality using Tantivy

use anyhow::{Context, Result};
use chrono::{DateTime, Duration, Utc};
use serde::Serialize;

use tantivy::collector::{Count, TopDocs};
use tantivy::query::{BooleanQuery, Occur, QueryParser, RangeQuery, TermQuery};
use tantivy::schema::Type;
use tantivy::ReloadPolicy;
use tantivy::{DocAddress, IndexReader, Order, Searcher, Term};

use crate::index::aggregator::{ErrorAggCollector, FieldCountCollector};

use crate::config::Config;
use crate::schema::{ErrorInfo, LogEntry, LogFields};

/// Result ordering. Time (newest first) is the default regardless of
/// whether a full-text query is present — predictable for agents.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, clap::ValueEnum)]
pub enum SortOrder {
    /// Newest first by timestamp
    #[default]
    Time,
    /// Best full-text match first (requires --query)
    Relevance,
}

/// Search options
#[derive(Debug, Clone, Default)]
pub struct SearchOptions {
    pub query: Option<String>,
    pub level: Option<String>,
    pub log_id: Option<String>,
    pub request_id: Option<String>,
    pub user_id: Option<String>,
    pub route: Option<String>,
    pub source: Option<String>,
    pub method: Option<String>,
    /// Exact parser-origin filter, e.g. "raw", "json" (see LogEntry::parse_format)
    pub parse_format: Option<String>,
    /// Inclusive status-code range, e.g. (500, 599)
    pub status: Option<(u16, u16)>,
    /// Only entries with duration_ms strictly greater than this
    pub slow_above: Option<f64>,
    pub start: Option<String>,
    pub end: Option<String>,
    pub last: Option<String>,
    pub limit: usize,
    pub offset: usize,
    pub sort: SortOrder,
}

/// Plain search output. Commands wrap this in the v1 response envelope
/// (`src/output.rs`); the searcher itself is envelope-agnostic.
#[derive(Debug)]
pub struct QueryOutput {
    pub results: Vec<LogEntry>,
    /// Total matching documents in the index (not just the returned page)
    pub total: usize,
    /// Filters that were applied, as a snake_case object for `meta.filters`
    pub filters: serde_json::Map<String, serde_json::Value>,
    /// `{"start", "end"}` of the returned results, when non-empty
    pub time_range: Option<serde_json::Value>,
}

/// Context-mode output: each matching entry with its surrounding timeline.
#[derive(Debug)]
pub struct ContextOutput {
    pub windows: Vec<ContextWindow>,
    pub total: usize,
    pub filters: serde_json::Map<String, serde_json::Value>,
    pub time_range: Option<serde_json::Value>,
}

/// A target log plus surrounding logs in chronological order.
#[derive(Debug, Serialize)]
pub struct ContextWindow {
    pub target: LogEntry,
    pub before: Vec<LogEntry>,
    pub after: Vec<LogEntry>,
}

/// Result of a streaming stats aggregation.
pub struct StatsQueryResult {
    /// Total number of matching documents
    pub total: usize,
    /// Counts by level (when requested)
    pub by_level: Option<HashMap<String, usize>>,
    /// Counts by route (when requested)
    pub by_route: Option<HashMap<String, usize>>,
    /// Counts by user_id (when requested)
    pub by_user: Option<HashMap<String, usize>>,
    /// Counts by parse_format (when requested)
    pub by_format: Option<HashMap<String, usize>>,
}

/// Result of a streaming error aggregation.
pub struct ErrorsQueryResult {
    /// Total number of error documents
    pub total_errors: usize,
    /// Error groups sorted by count descending
    pub groups: Vec<crate::index::aggregator::AggregatedErrorGroup>,
}

use crate::cmd::schema::IndexStats;
use std::collections::HashMap;

/// Search the index using Tantivy
pub struct IndexSearcher {
    reader: IndexReader,
    fields: LogFields,
}

impl IndexSearcher {
    /// Create a new searcher. A missing index is not an error: it is
    /// created empty on demand (searches then return zero results).
    pub fn new(config: Config) -> Result<Self> {
        let (index, fields) = crate::sync::open_or_create_index(&config)?;

        let reader = index
            .reader_builder()
            .reload_policy(ReloadPolicy::OnCommitWithDelay)
            .try_into()
            .context("Failed to create index reader")?;

        Ok(IndexSearcher { reader, fields })
    }

    /// Get summary statistics about the index
    pub fn get_index_stats(&self) -> Result<IndexStats> {
        let searcher = self.reader.searcher();
        let num_docs = searcher.num_docs() as usize;

        if num_docs == 0 {
            return Ok(IndexStats {
                total_entries: 0,
                oldest_entry: "".into(),
                newest_entry: "".into(),
                sources: Vec::new(),
                levels_count: HashMap::new(),
            });
        }

        // For now, we'll do a simple scan of a sample of documents to find stats
        let query = tantivy::query::AllQuery;
        let sample_limit = 10000;
        let all_docs = searcher.search(&query, &TopDocs::with_limit(sample_limit))?;

        let mut sources = std::collections::HashSet::new();
        let mut levels_count = HashMap::new();
        let mut oldest_entry = String::new();
        let mut newest_entry = String::new();

        for (_, doc_addr) in all_docs {
            if let Ok(entry) = self.document_to_entry(&searcher, doc_addr) {
                sources.insert(entry.source);
                *levels_count.entry(entry.level).or_insert(0) += 1;

                if oldest_entry.is_empty() || entry.timestamp < oldest_entry {
                    oldest_entry = entry.timestamp.clone();
                }
                if newest_entry.is_empty() || entry.timestamp > newest_entry {
                    newest_entry = entry.timestamp;
                }
            }
        }

        Ok(IndexStats {
            total_entries: num_docs,
            oldest_entry,
            newest_entry,
            sources: sources.into_iter().collect(),
            levels_count,
        })
    }

    /// Fetch up to `limit` of the newest entries (for field profiling).
    pub fn sample_entries(&self, limit: usize) -> Result<Vec<LogEntry>> {
        let searcher = self.reader.searcher();
        let docs = searcher.search(
            &tantivy::query::AllQuery,
            &TopDocs::with_limit(limit.max(1)).order_by_u64_field("timestamp", Order::Desc),
        )?;
        Ok(docs
            .into_iter()
            .filter_map(|(_, addr)| self.document_to_entry(&searcher, addr).ok())
            .collect())
    }

    // -----------------------------------------------------------------------
    // Query builder (shared between search & aggregation)
    // -----------------------------------------------------------------------

    /// Build a Tantivy query from `SearchOptions`.
    ///
    /// Returns `(query, filters)` — filters as a snake_case object for the
    /// envelope's `meta.filters` — so callers can choose their own
    /// collector: `TopDocs` for regular search, custom collectors for
    /// aggregation.
    fn build_query(
        &self,
        options: &SearchOptions,
    ) -> Result<(
        Box<dyn tantivy::query::Query>,
        serde_json::Map<String, serde_json::Value>,
    )> {
        let searcher = self.reader.searcher();
        let index = searcher.index();

        let mut queries: Vec<(Occur, Box<dyn tantivy::query::Query>)> = Vec::new();
        let mut filters = serde_json::Map::new();

        // Full-text query
        if let Some(ref query_text) = options.query {
            let query_parser =
                QueryParser::for_index(index, vec![self.fields.full_text, self.fields.message]);
            let query = query_parser
                .parse_query(query_text)
                .context("Failed to parse query")?;
            queries.push((Occur::Must, Box::new(query)));
            filters.insert("query".into(), query_text.clone().into());
        }

        // Exact-match term filters
        let term_filters: [(&str, &Option<String>, tantivy::schema::Field); 8] = [
            ("level", &options.level, self.fields.level),
            ("log_id", &options.log_id, self.fields.log_id),
            ("request_id", &options.request_id, self.fields.request_id),
            ("user_id", &options.user_id, self.fields.user_id),
            ("route", &options.route, self.fields.route),
            ("source", &options.source, self.fields.source),
            ("method", &options.method, self.fields.method),
            (
                "parse_format",
                &options.parse_format,
                self.fields.parse_format,
            ),
        ];
        for (name, value, field) in term_filters {
            if let Some(ref value) = value {
                let term = Term::from_field_text(field, value);
                let query = TermQuery::new(term, tantivy::schema::IndexRecordOption::Basic);
                queries.push((Occur::Must, Box::new(query)));
                filters.insert(name.into(), value.clone().into());
            }
        }

        // Status-code range filter (e.g. 500 or 500-599), inclusive
        if let Some((low, high)) = options.status {
            let lower = std::ops::Bound::Included(Term::from_field_u64(
                self.fields.status_code,
                low as u64,
            ));
            let upper = std::ops::Bound::Included(Term::from_field_u64(
                self.fields.status_code,
                high as u64,
            ));
            let query =
                RangeQuery::new_term_bounds("status_code".to_string(), Type::U64, &lower, &upper);
            queries.push((Occur::Must, Box::new(query)));
            filters.insert(
                "status".into(),
                if low == high {
                    low.to_string().into()
                } else {
                    format!("{}-{}", low, high).into()
                },
            );
        }

        // Slow-request filter: duration_ms > threshold
        if let Some(threshold) = options.slow_above {
            let lower =
                std::ops::Bound::Excluded(Term::from_field_f64(self.fields.duration_ms, threshold));
            let query = RangeQuery::new_term_bounds(
                "duration_ms".to_string(),
                Type::F64,
                &lower,
                &std::ops::Bound::Unbounded,
            );
            queries.push((Occur::Must, Box::new(query)));
            filters.insert("slow_above".into(), threshold.into());
        }

        // Time range filter
        if options.start.is_some() || options.end.is_some() || options.last.is_some() {
            if let Some(range_query) = self.build_time_range_query(options)? {
                queries.push((Occur::Must, Box::new(range_query)));
                if let Some(ref last) = options.last {
                    filters.insert("last".into(), last.clone().into());
                } else {
                    if let Some(ref start) = options.start {
                        filters.insert("start".into(), start.clone().into());
                    }
                    if let Some(ref end) = options.end {
                        filters.insert("end".into(), end.clone().into());
                    }
                }
            }
        }

        // Build final query
        let query: Box<dyn tantivy::query::Query> = if queries.is_empty() {
            Box::new(tantivy::query::AllQuery)
        } else {
            Box::new(BooleanQuery::new(queries))
        };

        Ok((query, filters))
    }

    // -----------------------------------------------------------------------
    // Streaming aggregation: stats
    // -----------------------------------------------------------------------

    /// Aggregate matching documents by level, route, and/or user using
    /// custom Tantivy collectors.
    ///
    /// Unlike calling `search()` with a 10K limit and then counting
    /// in-memory, this processes **all** matching documents during the
    /// search phase without materialising `LogEntry` objects.
    pub fn stats_query(
        &self,
        options: &SearchOptions,
        by_level: bool,
        by_route: bool,
        by_user: bool,
        by_format: bool,
    ) -> Result<StatsQueryResult> {
        let searcher = self.reader.searcher();
        let (query, _) = self.build_query(options)?;

        // Get total count
        let total = searcher.search(&query, &Count)?;

        // Per-field counts via columnar fast fields (see FieldCountCollector).
        // Each dimension is a separate pass, but each pass only reads term
        // ordinals from the column — cheap enough that N passes stay well under
        // the old single store-reading pass.
        let by_level_map = if by_level {
            Some(searcher.search(&query, &FieldCountCollector::new("level", ""))?)
        } else {
            None
        };
        let by_route_map = if by_route {
            Some(searcher.search(&query, &FieldCountCollector::new("route", "unknown"))?)
        } else {
            None
        };
        let by_user_map = if by_user {
            Some(searcher.search(&query, &FieldCountCollector::new("user_id", "anonymous"))?)
        } else {
            None
        };
        // Pre-v5 docs lack the column; unknown origin counts as "raw".
        let by_format_map = if by_format {
            Some(searcher.search(&query, &FieldCountCollector::new("parse_format", "raw"))?)
        } else {
            None
        };

        Ok(StatsQueryResult {
            total,
            by_level: by_level_map,
            by_route: by_route_map,
            by_user: by_user_map,
            by_format: by_format_map,
        })
    }

    // -----------------------------------------------------------------------
    // Streaming aggregation: error groups
    // -----------------------------------------------------------------------

    /// Aggregate error logs into groups by fingerprint during the search
    /// phase.
    ///
    /// Returns `(total_errors, sorted_error_groups)` where groups are
    /// sorted by count descending and truncated to `limit`.
    pub fn errors_query(&self, options: &SearchOptions, limit: usize) -> Result<ErrorsQueryResult> {
        let searcher = self.reader.searcher();
        let (query, _) = self.build_query(options)?;

        let collector = ErrorAggCollector::new(
            self.fields.error_name,
            self.fields.error_message,
            self.fields.user_id,
            self.fields.timestamp,
            self.fields.log_id,
            self.fields.message,
        );

        let groups = searcher.search(&query, &collector)?;

        // Calculate total errors and sort groups
        let total_errors: usize = groups.values().map(|g| g.count).sum();

        let mut sorted_groups: Vec<crate::index::aggregator::AggregatedErrorGroup> =
            groups.into_values().collect();
        // Count desc; ties broken by recency (single-failure runs produce
        // many count-1 groups — the latest failure is the relevant one),
        // then by template length (batch-ingested blocks share millisecond
        // timestamps, and the longer template carries the actual diagnostic,
        // not a preamble like "error during build:").
        sorted_groups.sort_by(|a, b| {
            b.count
                .cmp(&a.count)
                .then_with(|| b.last_seen.cmp(&a.last_seen))
                .then_with(|| b.template.len().cmp(&a.template.len()))
        });
        sorted_groups.truncate(limit);

        Ok(ErrorsQueryResult {
            total_errors,
            groups: sorted_groups,
        })
    }

    /// Search logs with the given options
    pub fn search(&self, options: SearchOptions) -> Result<QueryOutput> {
        let searcher = self.reader.searcher();

        // Build query using shared query builder
        let (query, filters) = self.build_query(&options)?;

        // True total of matching documents (independent of limit/offset)
        let total = searcher.search(&query, &Count).context("Count failed")?;

        // Execute search. Ordering is always newest-first unless relevance
        // was explicitly requested alongside a full-text query.
        let limit = options.limit.clamp(1, 10000);
        let by_relevance = options.sort == SortOrder::Relevance && options.query.is_some();
        let doc_addresses = if by_relevance {
            searcher
                .search(
                    &query,
                    &TopDocs::with_limit(limit).and_offset(options.offset),
                )
                .context("Search failed")?
                .into_iter()
                .map(|(_, doc_address)| doc_address)
                .collect::<Vec<_>>()
        } else {
            searcher
                .search(
                    &query,
                    &TopDocs::with_limit(limit)
                        .and_offset(options.offset)
                        .order_by_u64_field("timestamp", Order::Desc),
                )
                .context("Search failed")?
                .into_iter()
                .map(|(_, doc_address)| doc_address)
                .collect::<Vec<_>>()
        };

        // Collect results
        let mut results: Vec<LogEntry> = Vec::new();
        for doc_address in doc_addresses {
            if let Ok(entry) = self.document_to_entry(&searcher, doc_address) {
                results.push(entry);
            }
        }

        // Time range of the returned page (results are timestamp-ordered)
        let time_range = if results.is_empty() {
            None
        } else {
            let timestamps: Vec<&str> = results.iter().map(|r| r.timestamp.as_str()).collect();
            let start = timestamps.iter().min().copied().unwrap_or_default();
            let end = timestamps.iter().max().copied().unwrap_or_default();
            Some(serde_json::json!({"start": start, "end": end}))
        };

        Ok(QueryOutput {
            results,
            total,
            filters,
            time_range,
        })
    }

    /// Search matching target logs and include surrounding entries from the
    /// full log timeline for single-command root cause analysis.
    ///
    /// Each target's neighborhood is fetched with two bounded timestamp
    /// range queries (limit = context each) — O(targets × context) work
    /// instead of loading the entire index timeline into memory.
    pub fn search_with_context(
        &self,
        options: SearchOptions,
        context: usize,
    ) -> Result<ContextOutput> {
        let target_result = self.search(options)?;
        let searcher = self.reader.searcher();

        let windows = target_result
            .results
            .into_iter()
            .map(|target| {
                let before = self
                    .timeline_neighbors(&searcher, &target, context, Order::Desc)
                    .unwrap_or_default();
                let after = self
                    .timeline_neighbors(&searcher, &target, context, Order::Asc)
                    .unwrap_or_default();
                ContextWindow {
                    target,
                    before,
                    after,
                }
            })
            .collect();

        Ok(ContextOutput {
            windows,
            total: target_result.total,
            filters: target_result.filters,
            time_range: target_result.time_range,
        })
    }

    /// Fetch up to `count` timeline neighbors of `target`: entries strictly
    /// before (Order::Desc) or after (Order::Asc) its timestamp. Entries
    /// sharing the exact timestamp are included and the target itself is
    /// filtered out; results are returned in chronological order.
    fn timeline_neighbors(
        &self,
        searcher: &Searcher,
        target: &LogEntry,
        count: usize,
        direction: Order,
    ) -> Result<Vec<LogEntry>> {
        if count == 0 {
            return Ok(Vec::new());
        }

        let term = Term::from_field_text(self.fields.timestamp, &target.timestamp);
        let (lower, upper) = match direction {
            // Everything up to and including the target's timestamp
            Order::Desc => (std::ops::Bound::Unbounded, std::ops::Bound::Included(term)),
            // Everything from the target's timestamp on
            Order::Asc => (std::ops::Bound::Included(term), std::ops::Bound::Unbounded),
        };
        let range_query =
            RangeQuery::new_term_bounds("timestamp".to_string(), Type::Str, &lower, &upper);

        // +1 headroom for the target itself landing inside the range.
        let mut entries: Vec<LogEntry> = searcher
            .search(
                &range_query,
                &TopDocs::with_limit(count + 1).order_by_u64_field("timestamp", direction.clone()),
            )
            .context("Context window search failed")?
            .into_iter()
            .filter_map(|(_, doc_address)| self.document_to_entry(searcher, doc_address).ok())
            .filter(|entry| entry.log_id != target.log_id)
            .take(count)
            .collect();

        if direction == Order::Desc {
            entries.reverse(); // chronological order
        }
        Ok(entries)
    }

    /// Build a time range query from search options
    ///
    /// Supports:
    /// - `--last 24h` - Relative duration (e.g., 1h, 24h, 7d, 1w, 30m)
    /// - `--start` and `--end` - Absolute ISO8601 timestamps
    fn build_time_range_query(
        &self,
        options: &SearchOptions,
    ) -> Result<Option<Box<dyn tantivy::query::Query>>> {
        // Determine the time bounds
        let (start_time, end_time) = if let Some(ref last) = options.last {
            // Parse relative duration (e.g., "24h", "7d", "1w").
            // Deliberately no upper bound: entries with slightly-future
            // timestamps (clock skew between machines) must not vanish
            // from "recent" queries.
            let duration = parse_duration(last).context("Failed to parse --last duration")?;
            let start = Utc::now() - duration;
            (Some(start), None)
        } else {
            // Parse absolute timestamps
            let start = options
                .start
                .as_ref()
                .map(|s| parse_iso8601(s))
                .transpose()
                .context("Failed to parse --start timestamp")?;
            let end = options
                .end
                .as_ref()
                .map(|s| parse_iso8601(s))
                .transpose()
                .context("Failed to parse --end timestamp")?;
            (start, end)
        };

        // If no bounds, return None
        if start_time.is_none() && end_time.is_none() {
            return Ok(None);
        }

        // Numeric epoch-millis range over the `timestamp_ms` fast field: a
        // columnar comparison instead of walking the `timestamp` term dictionary
        // (~1 distinct term per doc on a large index). Mirrors the `status_code`
        // range filter above. Docs whose timestamp couldn't be parsed to millis
        // at ingest have no `timestamp_ms` and are excluded from time filters —
        // consistent with their un-normalizable string timestamp.
        let to_ms = |dt: DateTime<Utc>| dt.timestamp_millis().max(0) as u64;
        let lower = start_time
            .map(|dt| {
                std::ops::Bound::Included(Term::from_field_u64(self.fields.timestamp_ms, to_ms(dt)))
            })
            .unwrap_or(std::ops::Bound::Unbounded);
        let upper = end_time
            .map(|dt| {
                std::ops::Bound::Included(Term::from_field_u64(self.fields.timestamp_ms, to_ms(dt)))
            })
            .unwrap_or(std::ops::Bound::Unbounded);

        let range_query =
            RangeQuery::new_term_bounds("timestamp_ms".to_string(), Type::U64, &lower, &upper);

        Ok(Some(Box::new(range_query)))
    }

    /// Convert a document to a LogEntry
    fn document_to_entry(&self, searcher: &Searcher, doc_address: DocAddress) -> Result<LogEntry> {
        use tantivy::schema::{OwnedValue, TantivyDocument};

        let doc: TantivyDocument = searcher
            .doc(doc_address)
            .context("Failed to retrieve document")?;

        let get_text = |field: tantivy::schema::Field| -> Option<String> {
            doc.get_first(field).and_then(|v: &OwnedValue| match v {
                OwnedValue::Str(s) => Some(s.clone()),
                OwnedValue::PreTokStr(s) => Some(s.text.clone()),
                _ => None,
            })
        };

        let get_u64 = |field: tantivy::schema::Field| -> Option<u64> {
            doc.get_first(field).and_then(|v: &OwnedValue| match v {
                OwnedValue::U64(n) => Some(*n),
                OwnedValue::I64(n) => Some(*n as u64),
                _ => None,
            })
        };

        let get_f64 = |field: tantivy::schema::Field| -> Option<f64> {
            doc.get_first(field).and_then(|v: &OwnedValue| match v {
                OwnedValue::F64(n) => Some(*n),
                _ => None,
            })
        };

        let error_name = get_text(self.fields.error_name);
        let error_message = get_text(self.fields.error_message);

        Ok(LogEntry {
            log_id: get_text(self.fields.log_id).unwrap_or_default(),
            timestamp: get_text(self.fields.timestamp).unwrap_or_default(),
            level: get_text(self.fields.level).unwrap_or_default(),
            source: get_text(self.fields.source).unwrap_or_default(),
            message: get_text(self.fields.message).unwrap_or_default(),
            request_id: get_text(self.fields.request_id),
            user_id: get_text(self.fields.user_id),
            route: get_text(self.fields.route),
            method: get_text(self.fields.method),
            status_code: get_u64(self.fields.status_code).map(|v| v as u16),
            duration_ms: get_f64(self.fields.duration_ms),
            error: if error_name.is_some() || error_message.is_some() {
                Some(ErrorInfo {
                    name: error_name,
                    message: error_message,
                    code: None,
                })
            } else {
                None
            },
            source_file: get_text(self.fields.source_file),
            line_number: get_u64(self.fields.line_number).map(|v| v as usize),
            attributes: get_text(self.fields.attributes)
                .and_then(|json| serde_json::from_str(&json).ok()),
            // Pre-v5 documents have no parse_format; report them as "raw"
            // (unknown origin must not count as structured).
            parse_format: get_text(self.fields.parse_format).unwrap_or_else(|| "raw".to_string()),
        })
    }
}

// ============================================================================
// Time parsing utilities
// ============================================================================

/// Parse a relative duration string (e.g., "24h", "7d", "1w", "30m", "5s")
///
/// Supported units:
/// - `s` - seconds
/// - `m` - minutes
/// - `h` - hours
/// - `d` - days
/// - `w` - weeks
///
/// Returns an error if the format is invalid.
pub fn parse_duration(s: &str) -> Result<Duration> {
    let s = s.trim();
    if s.is_empty() {
        anyhow::bail!("Empty duration string");
    }

    // Parse the numeric part and unit
    let (num_str, unit) = if let Some(pos) = s.find(|c: char| !c.is_ascii_digit()) {
        let (num, unit) = s.split_at(pos);
        (num, unit)
    } else {
        anyhow::bail!("Duration string missing unit: {}", s);
    };

    let num: i64 = num_str
        .parse()
        .with_context(|| format!("Invalid duration number: {}", num_str))?;

    let duration = match unit.to_lowercase().as_str() {
        "s" | "sec" | "secs" | "second" | "seconds" => Duration::seconds(num),
        "m" | "min" | "mins" | "minute" | "minutes" => Duration::minutes(num),
        "h" | "hour" | "hours" => Duration::hours(num),
        "d" | "day" | "days" => Duration::days(num),
        "w" | "week" | "weeks" => Duration::weeks(num),
        _ => anyhow::bail!("Unknown duration unit: {}. Use s, m, h, d, or w", unit),
    };

    Ok(duration)
}

/// Parse an ISO8601 timestamp string into a DateTime
///
/// Supports formats:
/// - `2024-01-15T10:30:00.123Z` (with milliseconds)
/// - `2024-01-15T10:30:00Z` (without milliseconds)
/// - `2024-01-15T10:30:00+00:00` (with timezone offset)
pub fn parse_iso8601(s: &str) -> Result<DateTime<Utc>> {
    let s = s.trim();

    // Try parsing with various formats
    let dt = chrono::DateTime::parse_from_rfc3339(s)
        .with_context(|| format!("Invalid ISO8601 timestamp: {}", s))?
        .with_timezone(&Utc);

    Ok(dt)
}

/// Format a DateTime to ISO8601 with milliseconds (matching our log format)
///
/// Output format: `2024-01-15T10:30:00.123Z`
pub fn format_timestamp(dt: &DateTime<Utc>) -> String {
    dt.format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{Datelike, Timelike};

    #[test]
    fn test_parse_duration_seconds() {
        let dur = parse_duration("30s").unwrap();
        assert_eq!(dur, Duration::seconds(30));

        let dur = parse_duration("1sec").unwrap();
        assert_eq!(dur, Duration::seconds(1));

        let dur = parse_duration("60seconds").unwrap();
        assert_eq!(dur, Duration::seconds(60));
    }

    #[test]
    fn test_parse_duration_minutes() {
        let dur = parse_duration("5m").unwrap();
        assert_eq!(dur, Duration::minutes(5));

        let dur = parse_duration("30min").unwrap();
        assert_eq!(dur, Duration::minutes(30));

        let dur = parse_duration("60minutes").unwrap();
        assert_eq!(dur, Duration::minutes(60));
    }

    #[test]
    fn test_parse_duration_hours() {
        let dur = parse_duration("1h").unwrap();
        assert_eq!(dur, Duration::hours(1));

        let dur = parse_duration("24h").unwrap();
        assert_eq!(dur, Duration::hours(24));

        let dur = parse_duration("48hours").unwrap();
        assert_eq!(dur, Duration::hours(48));
    }

    #[test]
    fn test_parse_duration_days() {
        let dur = parse_duration("1d").unwrap();
        assert_eq!(dur, Duration::days(1));

        let dur = parse_duration("7d").unwrap();
        assert_eq!(dur, Duration::days(7));

        let dur = parse_duration("30days").unwrap();
        assert_eq!(dur, Duration::days(30));
    }

    #[test]
    fn test_parse_duration_weeks() {
        let dur = parse_duration("1w").unwrap();
        assert_eq!(dur, Duration::weeks(1));

        let dur = parse_duration("2weeks").unwrap();
        assert_eq!(dur, Duration::weeks(2));
    }

    #[test]
    fn test_parse_duration_invalid() {
        assert!(parse_duration("").is_err());
        assert!(parse_duration("invalid").is_err());
        assert!(parse_duration("5x").is_err());
        assert!(parse_duration("h").is_err());
    }

    #[test]
    fn test_parse_iso8601_with_millis() {
        let dt = parse_iso8601("2024-01-15T10:30:00.123Z").unwrap();
        assert_eq!(dt.year(), 2024);
        assert_eq!(dt.month(), 1);
        assert_eq!(dt.day(), 15);
        assert_eq!(dt.hour(), 10);
        assert_eq!(dt.minute(), 30);
        assert_eq!(dt.second(), 0);
    }

    #[test]
    fn test_parse_iso8601_without_millis() {
        let dt = parse_iso8601("2024-01-15T10:30:00Z").unwrap();
        assert_eq!(dt.year(), 2024);
        assert_eq!(dt.month(), 1);
        assert_eq!(dt.day(), 15);
        assert_eq!(dt.hour(), 10);
        assert_eq!(dt.minute(), 30);
        assert_eq!(dt.second(), 0);
    }

    #[test]
    fn test_parse_iso8601_with_timezone() {
        let dt = parse_iso8601("2024-01-15T10:30:00+00:00").unwrap();
        assert_eq!(dt.year(), 2024);
        assert_eq!(dt.month(), 1);
        assert_eq!(dt.day(), 15);
    }

    #[test]
    fn test_parse_iso8601_invalid() {
        assert!(parse_iso8601("invalid").is_err());
        assert!(parse_iso8601("2024-01-15").is_err());
        assert!(parse_iso8601("").is_err());
    }

    #[test]
    fn test_format_timestamp() {
        let dt = parse_iso8601("2024-01-15T10:30:00.123Z").unwrap();
        let formatted = format_timestamp(&dt);
        assert_eq!(formatted, "2024-01-15T10:30:00.123Z");

        // Test that formatting adds millis if not present
        let dt = parse_iso8601("2024-01-15T10:30:00Z").unwrap();
        let formatted = format_timestamp(&dt);
        assert!(formatted.ends_with("Z"));
        assert!(formatted.contains("."));
    }

    #[test]
    fn test_timestamp_lexicographic_ordering() {
        // ISO8601 timestamps should sort correctly as strings
        let ts1 = "2024-01-15T10:00:00.000Z";
        let ts2 = "2024-01-15T10:30:00.000Z";
        let ts3 = "2024-01-15T11:00:00.000Z";
        let ts4 = "2024-01-16T10:00:00.000Z";

        assert!(ts1 < ts2);
        assert!(ts2 < ts3);
        assert!(ts3 < ts4);
    }

    #[test]
    fn test_time_range_filtering_logic() {
        // Test that duration subtraction produces correct bounds
        let now = Utc::now();
        let one_hour_ago = now - Duration::hours(1);

        // The start should be before the end
        assert!(one_hour_ago < now);

        // The difference should be approximately 1 hour
        let diff = now - one_hour_ago;
        assert_eq!(diff.num_hours(), 1);
    }
}
