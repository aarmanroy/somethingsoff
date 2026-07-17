//! Watch command - continuous ingestion loop over the shared sync engine.
//!
//! Optional: read commands auto-sync on their own. `watch` exists for
//! lower-latency freshness (poll interval instead of on-demand) and to keep
//! ingesting while no queries are running. Because it persists cursor state
//! after every poll, concurrent read commands hit the sync fast path and
//! never contend for the writer lock.

use anyhow::Result;
use clap::Args;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use crate::config::Config;
use crate::sync::lock::SyncLock;
use crate::sync::state::SyncState;
use crate::sync::{discover, open_or_create_index, run_sync};

const LOCK_TIMEOUT: Duration = Duration::from_secs(5);

/// Continuously ingest new log entries (optional; reads auto-sync anyway)
#[derive(Args)]
pub struct WatchCommand {
    /// Deprecated no-op, kept so `somethingsoff serve --watch` still works
    #[arg(long = "watch", hide = true)]
    pub watch: bool,

    /// Polling interval in seconds (default: config sync.poll_interval_secs)
    #[arg(long = "interval")]
    pub interval_secs: Option<u64>,

    /// Watch a single log file (otherwise: all discovered sources)
    #[arg(short, long)]
    pub file: Option<PathBuf>,

    /// Source name for --file (required together with --file)
    #[arg(short, long)]
    pub source: Option<String>,
}

impl WatchCommand {
    pub async fn execute(&self) -> Result<()> {
        let config = Config::load()?;
        let base_dir = crate::config::base_dir();

        // Validate the explicit source/file pair
        let explicit: Option<(String, PathBuf)> =
            match (&self.source, &self.file) {
                (Some(source), Some(file)) => Some((source.clone(), file.clone())),
                (None, None) => None,
                _ => anyhow::bail!(
                    "Both --file and --source must be specified together, or neither to watch all discovered sources"
                ),
            };

        // Hold the writer lock for the process lifetime.
        let _lock = SyncLock::acquire_blocking(&base_dir, LOCK_TIMEOUT)?;
        if crate::sync::needs_migration(&config) {
            crate::sync::migrate_index(&config)?;
        }
        let (index, fields) = open_or_create_index(&config)?;
        let mut writer = index
            .writer_with_num_threads(1, 50_000_000)
            .map_err(|e| anyhow::anyhow!("Failed to create index writer: {}", e))?;

        let interval = Duration::from_secs(
            self.interval_secs
                .unwrap_or(config.sync.poll_interval_secs)
                .max(1),
        );

        crate::log_info!("Watching for new log entries every {:?}", interval);
        crate::log_info!("Press Ctrl+C to stop");

        loop {
            let started = Instant::now();

            // Re-discover every poll so files that appear later are picked up.
            let sources = match &explicit {
                Some(pair) => vec![pair.clone()],
                None => {
                    let state = SyncState::load(&base_dir);
                    discover::discover_sources(&config, &state)
                }
            };

            if sources.is_empty() {
                crate::log_info!(
                    "No log sources found yet (drop files into ./logs/ or configure sources)"
                );
            } else if let Err(e) = run_sync(&config, &sources, &mut writer, &fields, started) {
                crate::log_error!("Sync pass failed: {}", e);
            }

            tokio::time::sleep(interval).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_watch_command_defaults() {
        let cmd = WatchCommand {
            watch: false,
            interval_secs: None,
            file: None,
            source: None,
        };
        assert!(cmd.interval_secs.is_none());
        assert!(cmd.file.is_none());
    }

    #[test]
    fn test_watch_command_with_file() {
        let cmd = WatchCommand {
            watch: true,
            interval_secs: Some(5),
            file: Some(PathBuf::from("logs/test.log")),
            source: Some("test".to_string()),
        };
        assert_eq!(cmd.interval_secs, Some(5));
        assert_eq!(cmd.file.unwrap(), PathBuf::from("logs/test.log"));
        assert_eq!(cmd.source.unwrap(), "test");
    }
}
