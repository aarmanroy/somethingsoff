//! Patterns command - mine recurring message templates (Drain clustering)
//!
//! Collapses noisy log windows into a ranked list of message templates
//! ("what is the app logging?"), across all levels — the triage
//! counterpart to `errors`, which only groups error events. Uses the
//! native Drain implementation in `crate::drain` over the newest matching
//! entries; templates are recomputed per query (no persisted tree).

use anyhow::Result;
use clap::Args;
use serde::Serialize;
use std::collections::HashMap;

use crate::config::Config;
use crate::index::searcher::SearchOptions;
use crate::output::{list_meta, Envelope};

/// Hard cap on documents fed into one Drain pass. Bounds memory (~first
/// lines only) and keeps the command interactive on large windows; a
/// `sampled` notice is attached when the cap bites.
const PATTERNS_SCAN_CAP: usize = 50_000;

/// Mine recurring message templates (Drain clustering)
#[derive(Args)]
pub struct PatternsCommand {
    /// Time window (5m, 1h, 1d, 7d)
    #[arg(short, long, default_value = "24h")]
    pub last: String,

    /// Maximum number of templates to return
    #[arg(short = 'n', long, default_value = "20")]
    pub limit: usize,

    /// Filter by source name (e.g. backend, api)
    #[arg(long)]
    pub source: Option<String>,

    /// Filter by log level (debug, info, warn, error)
    #[arg(short = 'L', long)]
    pub level: Option<String>,
}

/// One mined template with aggregate statistics
#[derive(Debug, Serialize)]
pub struct PatternGroup {
    /// Stable ID for this template string ("v1:" + sha256 prefix).
    /// Stable within a response; a different window can mine different
    /// templates for the same message (unlike the errors fingerprint).
    pub template_id: String,
    /// Human-readable template; `<*>` marks Drain-generalized tokens
    pub template: String,
    /// Occurrences among the scanned entries
    pub count: usize,
    /// Percentage of scanned entries matching this template
    pub share_pct: f64,
    /// First occurrence timestamp (within the scanned window)
    pub first_seen: String,
    /// Most recent occurrence timestamp
    pub last_seen: String,
    /// Sample raw message (first line, first occurrence)
    pub sample_message: String,
    /// Sample log IDs for follow-up `get` calls
    pub sample_log_ids: Vec<String>,
    /// Occurrences per log level
    pub levels: HashMap<String, usize>,
}

impl PatternsCommand {
    pub async fn execute(&self) -> Result<u8> {
        let mut envelope = Envelope::new("patterns");
        let config = Config::load()?;
        let (searcher, sync_report) = crate::cmd::prepare_read(config)?;
        envelope.set_sync(sync_report);

        let options = SearchOptions {
            level: self.level.as_ref().map(|l| l.to_lowercase()),
            source: self.source.as_ref().map(|s| s.to_lowercase()),
            last: Some(self.last.clone()),
            ..Default::default()
        };

        let result = searcher.patterns_query(&options, self.limit, PATTERNS_SCAN_CAP)?;

        let scanned = result.scanned;
        let total_logs = result.total_logs;
        if total_logs > scanned {
            envelope.notice(
                "sampled",
                &format!(
                    "templates computed from the newest {} of {} matching entries",
                    scanned, total_logs
                ),
                Some("Narrow the window (--last) or filter (--level, --source) for full coverage."),
            );
        }

        let groups: Vec<PatternGroup> = result
            .templates
            .iter()
            .map(|c| PatternGroup {
                template_id: c.template_id(),
                template: c.template(),
                count: c.count,
                share_pct: if scanned == 0 {
                    0.0
                } else {
                    (c.count as f64 * 1000.0 / scanned as f64).round() / 10.0
                },
                first_seen: c.first_seen.clone(),
                last_seen: c.last_seen.clone(),
                sample_message: c.sample_message.clone(),
                sample_log_ids: c.sample_log_ids.clone(),
                levels: c.level_counts.clone(),
            })
            .collect();

        let total_templates = groups.len();

        if crate::is_jsonl() {
            envelope.emit_jsonl(&groups)?;
        } else {
            let meta = list_meta(
                total_templates,
                total_logs,
                Some(self.limit),
                None,
                serde_json::Value::Object(result.filters),
                None,
            );
            envelope.emit(
                serde_json::json!({
                    "total_logs": total_logs,
                    "scanned": scanned,
                    "total_templates": total_templates,
                    "templates": groups,
                }),
                Some(meta),
            )?;
        }

        Ok(if total_logs == 0 { 2 } else { 0 })
    }
}
