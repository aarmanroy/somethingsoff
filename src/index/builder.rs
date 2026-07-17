//! Index builder: full (re)build over all discovered sources.
//!
//! Thin wrapper over the shared sync engine — resets all sync cursors to 0
//! and runs one sync pass, so a rebuild ingests every source from scratch
//! (dedup makes overlap harmless).

use anyhow::{Context, Result};
use std::time::{Duration, Instant};

use crate::config::Config;
use crate::sync::lock::SyncLock;
use crate::sync::state::SyncState;
use crate::sync::{discover, open_or_create_index, run_sync};

const LOCK_TIMEOUT: Duration = Duration::from_secs(5);

/// Statistics from an index build operation
#[derive(Debug, Clone)]
pub struct BuildStats {
    pub files_processed: usize,
    pub files_failed: usize,
    pub entries_indexed: u64,
    pub size_mb: f64,
}

/// Index builder for creating and populating the search index
pub struct IndexBuilder {
    config: Config,
}

impl IndexBuilder {
    /// Create a new IndexBuilder with the given configuration
    pub fn new(config: Config) -> Self {
        IndexBuilder { config }
    }

    /// Build the index from all discovered log sources (config + ./logs +
    /// previously tracked files), re-reading every file from the start.
    pub fn build(&self) -> Result<BuildStats> {
        let start = Instant::now();
        let base_dir = crate::config::base_dir();
        let _lock = SyncLock::acquire_blocking(&base_dir, LOCK_TIMEOUT)?;

        // A rebuild always starts from a schema-current, empty-or-absent
        // index; if the on-disk schema is stale, remove it outright.
        if crate::sync::needs_migration(&self.config) {
            let index_dir = self.config.index_dir();
            std::fs::remove_dir_all(index_dir)
                .with_context(|| format!("Failed to remove old index: {:?}", index_dir))?;
        }

        // Reset cursors so every source is re-read from offset 0.
        let mut state = SyncState::load(&base_dir);
        state.reset_offsets();
        state.save(&base_dir)?;

        let sources = discover::discover_sources(&self.config, &state);
        let files_processed = sources.iter().filter(|(_, p)| p.exists()).count();
        let files_failed = sources.len() - files_processed;
        for (name, path) in sources.iter().filter(|(_, p)| !p.exists()) {
            crate::log_warn!("Log source path does not exist: {} ({:?})", name, path);
        }

        let (index, fields) = open_or_create_index(&self.config)?;
        let mut writer = index
            .writer_with_num_threads(1, 50_000_000)
            .context("Failed to create index writer")?;

        let report = run_sync(&self.config, &sources, &mut writer, &fields, start)?;
        writer
            .wait_merging_threads()
            .context("Failed to wait for merge")?;

        let index_dir = self.config.index_dir();
        let mut size_mb = get_dir_size(index_dir) as f64 / (1024.0 * 1024.0);
        size_mb = (size_mb * 100.0).round() / 100.0;

        crate::log_info!(
            "Index build complete: {} entries in {:.2}s ({:.2} MB)",
            report.ingested,
            start.elapsed().as_secs_f64(),
            size_mb
        );

        Ok(BuildStats {
            files_processed,
            files_failed,
            entries_indexed: report.ingested,
            size_mb,
        })
    }
}

/// Calculate the total size of a directory
pub fn get_dir_size(path: &std::path::Path) -> u64 {
    let mut size = 0;
    if path.is_dir() {
        if let Ok(entries) = std::fs::read_dir(path) {
            for entry in entries.flatten() {
                if let Ok(metadata) = entry.metadata() {
                    if metadata.is_file() {
                        size += metadata.len();
                    } else if metadata.is_dir() {
                        size += get_dir_size(&entry.path());
                    }
                }
            }
        }
    }
    size
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_build_stats_defaults() {
        let stats = BuildStats {
            files_processed: 0,
            files_failed: 0,
            entries_indexed: 0,
            size_mb: 0.0,
        };

        assert_eq!(stats.files_processed, 0);
        assert_eq!(stats.files_failed, 0);
        assert_eq!(stats.entries_indexed, 0);
        assert_eq!(stats.size_mb, 0.0);
    }

    #[test]
    fn test_get_dir_size() {
        let temp_dir = TempDir::new().expect("Failed to create temp dir");
        let size = get_dir_size(temp_dir.path());
        assert_eq!(size, 0, "Empty directory should have size 0");
    }
}
