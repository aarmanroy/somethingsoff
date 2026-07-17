//! Stats command - aggregated statistics using streaming Tantivy collectors.
//!
//! Instead of loading up to 10K docs into memory as `LogEntry` objects and
//! then counting, this uses custom `FieldCountCollector` instances that
//! aggregate during the Tantivy search phase — processing **all** matching
//! documents without the 10K limit.

use anyhow::Result;
use clap::Args;

use crate::config::Config;
use crate::index::searcher::SearchOptions;
use crate::output::Envelope;

/// Show aggregated statistics
#[derive(Args)]
pub struct StatsCommand {
    /// Count by log level
    #[arg(long = "by-level")]
    pub by_level: bool,

    /// Count by route
    #[arg(long = "by-route")]
    pub by_route: bool,

    /// Count by user
    #[arg(long = "by-user")]
    pub by_user: bool,

    /// Count by parse origin ("json", "syslog", ...; "raw" = unparsed fallback).
    /// A high raw share means field filters only see part of the data.
    #[arg(long = "by-format")]
    pub by_format: bool,

    /// Only error logs
    #[arg(long = "errors-only")]
    pub errors_only: bool,

    /// Time window (5m, 1h, 1d, 7d)
    #[arg(short, long)]
    pub last: Option<String>,
}

/// Helper: convert a `HashMap<String, usize>` to a count-descending map.
fn counts_to_json_map(
    counts: std::collections::HashMap<String, usize>,
) -> serde_json::Map<String, serde_json::Value> {
    let mut map = serde_json::Map::new();
    let mut entries: Vec<_> = counts.into_iter().collect();
    entries.sort_by(|a, b| b.1.cmp(&a.1));
    for (key, count) in entries {
        map.insert(key, serde_json::Value::Number(count.into()));
    }
    map
}

impl StatsCommand {
    pub async fn execute(&self) -> Result<u8> {
        let mut envelope = Envelope::new("stats");
        let config = Config::load()?;
        let (searcher, sync_report) = crate::cmd::prepare_read(config)?;
        envelope.set_sync(sync_report);

        let level = if self.errors_only {
            Some("error".to_string())
        } else {
            None
        };

        let options = SearchOptions {
            level: level.clone(),
            last: self.last.clone(),
            ..Default::default()
        };

        // If no aggregation flags are set, default to by-level
        let by_level = self.by_level || (!self.by_route && !self.by_user && !self.by_format);
        let by_route = self.by_route;
        let by_user = self.by_user;
        let by_format = self.by_format;

        // Use streaming aggregation — processes ALL matching docs
        let agg_result = searcher.stats_query(&options, by_level, by_route, by_user, by_format)?;

        let mut data = serde_json::Map::new();
        data.insert("total_logs".into(), agg_result.total.into());
        if let Some(counts) = agg_result.by_level {
            data.insert(
                "by_level".into(),
                serde_json::Value::Object(counts_to_json_map(counts)),
            );
        }
        if let Some(counts) = agg_result.by_route {
            data.insert(
                "by_route".into(),
                serde_json::Value::Object(counts_to_json_map(counts)),
            );
        }
        if let Some(counts) = agg_result.by_user {
            data.insert(
                "by_user".into(),
                serde_json::Value::Object(counts_to_json_map(counts)),
            );
        }
        if let Some(counts) = agg_result.by_format {
            data.insert(
                "by_format".into(),
                serde_json::Value::Object(counts_to_json_map(counts)),
            );
        }
        let data = serde_json::Value::Object(data);

        let mut filters = serde_json::Map::new();
        if let Some(level) = level {
            filters.insert("level".into(), level.into());
        }
        if let Some(ref last) = self.last {
            filters.insert("last".into(), last.clone().into());
        }
        let meta = serde_json::json!({ "filters": filters });

        if crate::is_jsonl() {
            // Stats is a single aggregate: jsonl emits the bare data object.
            envelope.emit_jsonl(&[&data])?;
        } else {
            envelope.emit(&data, Some(meta))?;
        }

        Ok(if agg_result.total == 0 { 2 } else { 0 })
    }
}
