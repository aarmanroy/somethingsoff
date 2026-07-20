//! Error aggregation command - group and count errors by fingerprint
//!
//! Uses a custom Tantivy collector (`ErrorAggCollector`) to group errors
//! during the search phase — processing **all** matching error documents
//! without materialising `LogEntry` objects or imposing a 10K limit.

use anyhow::Result;
use clap::Args;
use serde::Serialize;

use crate::config::Config;
use crate::index::searcher::SearchOptions;
use crate::output::{list_meta, Envelope};

// Re-exported from `crate::mask` for existing callers (benches, external
// users of the library crate).
pub use crate::mask::{generate_fingerprint, normalize_template};

/// Aggregate and analyze error logs
#[derive(Args)]
pub struct ErrorsCommand {
    /// Time window (5m, 1h, 1d, 7d)
    #[arg(short, long, default_value = "24h")]
    pub last: String,

    /// Maximum number of error groups to return
    #[arg(short = 'n', long, default_value = "10")]
    pub limit: usize,
}

/// Error group with aggregated statistics
#[derive(Debug, Serialize)]
pub struct ErrorGroup {
    /// Fingerprint hash (error name + masked template)
    pub fingerprint: String,
    /// Error name/type
    pub error_name: Option<String>,
    /// Sample error message (first seen in the group)
    pub error_message: Option<String>,
    /// Masked message template shared by the whole group
    pub template: String,
    /// Total occurrence count
    pub count: usize,
    /// Number of unique affected users
    pub affected_users: usize,
    /// First occurrence timestamp
    pub first_seen: String,
    /// Most recent occurrence timestamp
    pub last_seen: String,
    /// Sample log IDs for reference
    pub sample_log_ids: Vec<String>,
}

impl ErrorsCommand {
    pub async fn execute(&self) -> Result<u8> {
        let mut envelope = Envelope::new("errors");
        let config = Config::load()?;
        let (searcher, sync_report) = crate::cmd::prepare_read(config)?;
        envelope.set_sync(sync_report);

        let options = SearchOptions {
            level: Some("error".to_string()),
            last: Some(self.last.clone()),
            ..Default::default()
        };

        // Use streaming error aggregation (no 10K limit)
        let result = searcher.errors_query(&options, self.limit)?;

        // Convert aggregated groups to output format
        let error_groups: Vec<ErrorGroup> = result
            .groups
            .into_iter()
            .map(|g| ErrorGroup {
                fingerprint: g.fingerprint,
                error_name: g.error_name,
                error_message: g.error_message,
                template: g.template,
                count: g.count,
                affected_users: g.affected_users.len(),
                first_seen: g.first_seen,
                last_seen: g.last_seen,
                sample_log_ids: g.sample_log_ids,
            })
            .collect();

        let total_errors = result.total_errors;
        let total_groups = error_groups.len();

        if crate::is_jsonl() {
            envelope.emit_jsonl(&error_groups)?;
        } else {
            let meta = list_meta(
                total_groups,
                total_errors,
                Some(self.limit),
                None,
                serde_json::json!({"level": "error", "last": self.last}),
                None,
            );
            envelope.emit(
                serde_json::json!({
                    "total_errors": total_errors,
                    "total_groups": total_groups,
                    "groups": error_groups,
                }),
                Some(meta),
            )?;
        }

        Ok(if total_errors == 0 { 2 } else { 0 })
    }
}
