//! Command implementations

pub mod claude;
pub mod claude_hook;
pub mod errors;
pub mod get;
pub mod health;
pub mod index_cmd;
pub mod ingest;
pub mod learn;
pub mod patterns;
pub mod schema;
pub mod search;
pub mod stats;
pub mod tap;
pub mod watch;

pub use claude::ClaudeCommand;
pub use errors::ErrorsCommand;
pub use get::GetCommand;
pub use health::HealthCommand;
pub use index_cmd::IndexCommand;
pub use ingest::IngestCommand;
pub use learn::LearnCommand;
pub use patterns::PatternsCommand;
pub use schema::SchemaCommand;
pub use search::SearchCommand;
pub use stats::StatsCommand;
pub use tap::TapCommand;
pub use watch::WatchCommand;

use crate::config::Config;
use crate::index::searcher::IndexSearcher;
use crate::sync::SyncReport;
use anyhow::Result;

/// Shared entry point for read commands: transparently ingest any new log
/// data (unless disabled), then open a searcher on the fresh index.
pub fn prepare_read(config: Config) -> Result<(IndexSearcher, SyncReport)> {
    let report = crate::sync::sync_before_read(&config)?;
    if !report.skipped && report.ingested > 0 {
        crate::log_info!(
            "auto-sync: {} new entries from {} file(s) in {:.1}ms",
            report.ingested,
            report.files_checked,
            report.elapsed_ms
        );
    }
    let searcher = IndexSearcher::new(config)?;
    Ok((searcher, report))
}
