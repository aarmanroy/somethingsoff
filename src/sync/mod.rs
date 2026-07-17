//! Auto-ingest sync engine: keeps the index fresh transparently.
//!
//! Read commands call [`sync_before_read`] before querying, so
//! `somethingsoff search` "just works" in a fresh project: the index is
//! created on demand and only new bytes are ingested. When nothing changed
//! the fast path costs one state-file read plus one stat per source.
//!
//! Invariants:
//! - One writer at a time (the [`lock::SyncLock`] at `<base_dir>/.lock`).
//! - Readers never block: if the lock is busy the read proceeds against the
//!   current index (stale by at most one poll interval when `watch` runs).
//! - State is disposable: dedup-by-log_id makes re-ingestion from 0 correct.

pub mod discover;
pub mod lock;
pub mod state;
pub mod tail;

use anyhow::{Context, Result};
use chrono::Utc;
use serde::Serialize;
use std::path::PathBuf;
use std::time::Instant;

use crate::config::Config;
use crate::schema::{create_schema, LogFields, SCHEMA_VERSION};
use lock::SyncLock;
use state::{FileState, SyncState};
use tail::FileCursor;

/// What a sync pass did (surfaced in command output and stderr).
#[derive(Debug, Clone, Serialize, Default)]
pub struct SyncReport {
    /// True when no ingestion ran
    pub skipped: bool,
    /// Why it was skipped: "fresh" | "locked" | "disabled"
    pub reason: Option<String>,
    pub files_checked: usize,
    pub ingested: u64,
    pub failed: u64,
    pub elapsed_ms: f64,
    /// True when the index was transparently rebuilt for a schema upgrade
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub migrated: bool,
}

impl SyncReport {
    fn skipped(reason: &str, files_checked: usize, started: Instant) -> SyncReport {
        SyncReport {
            skipped: true,
            reason: Some(reason.to_string()),
            files_checked,
            ingested: 0,
            failed: 0,
            elapsed_ms: elapsed_ms(started),
            migrated: false,
        }
    }
}

fn elapsed_ms(started: Instant) -> f64 {
    (started.elapsed().as_secs_f64() * 1000.0 * 100.0).round() / 100.0
}

/// Open (creating if needed) the index at the configured location.
/// Stamps the schema version file on creation.
pub fn open_or_create_index(config: &Config) -> Result<(tantivy::Index, LogFields)> {
    let schema = create_schema();
    let fields = LogFields::new(&schema)?;
    let index_dir = config.index_dir();
    std::fs::create_dir_all(index_dir)
        .with_context(|| format!("Failed to create index directory: {:?}", index_dir))?;
    let directory = tantivy::directory::MmapDirectory::open(index_dir)
        .with_context(|| format!("Failed to open index directory: {:?}", index_dir))?;
    let index =
        tantivy::Index::open_or_create(directory, schema).context("Failed to create/open index")?;
    let version_path = index_dir.join("schema_version");
    if !version_path.exists() {
        let _ = std::fs::write(&version_path, SCHEMA_VERSION.to_string());
    }
    Ok((index, fields))
}

/// Does the on-disk index predate the current schema?
pub fn needs_migration(config: &Config) -> bool {
    let index_dir = config.index_dir();
    if !index_dir.join("meta.json").exists() {
        return false; // nothing to migrate
    }
    let disk_version = std::fs::read_to_string(index_dir.join("schema_version"))
        .ok()
        .and_then(|s| s.trim().parse::<u32>().ok());
    disk_version != Some(SCHEMA_VERSION)
}

/// Delete the index and reset all cursors so the next sync re-ingests
/// everything under the new schema. Caller MUST hold the writer lock.
pub fn migrate_index(config: &Config) -> Result<()> {
    crate::log_info!(
        "Index schema changed (now v{}); rebuilding transparently from sources",
        SCHEMA_VERSION
    );
    let index_dir = config.index_dir();
    if index_dir.exists() {
        std::fs::remove_dir_all(index_dir)
            .with_context(|| format!("Failed to remove old index: {:?}", index_dir))?;
    }
    let base_dir = crate::config::base_dir();
    let mut state = SyncState::load(&base_dir);
    state.reset_offsets();
    state.save(&base_dir)?;
    Ok(())
}

/// Is this file unchanged since its recorded state? (Pure stat check.)
fn is_fresh(path: &std::path::Path, file_state: Option<&FileState>) -> bool {
    let Some(fs) = file_state else {
        // Unknown file: fresh only if it doesn't exist (nothing to ingest).
        return !path.exists();
    };
    match tail::file_meta(path) {
        Ok((size, mtime_ms)) => size == fs.size && mtime_ms == fs.mtime_ms,
        // Missing/unreadable: nothing we can ingest — treat as fresh.
        Err(_) => true,
    }
}

/// Sync-on-read entry point. Never blocks on the lock; cheap when fresh.
pub fn sync_before_read(config: &Config) -> Result<SyncReport> {
    let started = Instant::now();

    if crate::is_no_sync() || !config.sync.auto {
        return Ok(SyncReport::skipped("disabled", 0, started));
    }

    let base_dir = crate::config::base_dir();
    let stat = SyncState::load(&base_dir);
    let sources = discover::discover_sources(config, &stat);
    let migrating = needs_migration(config);

    // Fast path: index exists, schema current, every source unchanged.
    let index_exists = config.index_dir().join("meta.json").exists();
    if !migrating
        && index_exists
        && sources
            .iter()
            .all(|(_, path)| is_fresh(path, stat.get(path)))
    {
        return Ok(SyncReport::skipped("fresh", sources.len(), started));
    }

    // Something to do: take the lock, or degrade to a stale read.
    let Some(_lock) = SyncLock::try_acquire(&base_dir)? else {
        if migrating {
            // A stale read is impossible against a mismatched schema.
            return Err(crate::output::CliError::new(
                crate::output::ErrorCode::IndexLocked,
                "The index needs a schema upgrade, but another process holds the writer lock",
            )
            .with_hint("Stop the running `somethingsoff watch`/`tap` (it will migrate on restart), then retry.")
            .into());
        }
        return Ok(SyncReport::skipped("locked", sources.len(), started));
    };

    if migrating {
        migrate_index(config)?;
    }

    let (index, fields) = open_or_create_index(config)?;
    let mut writer = index
        .writer_with_num_threads(1, 50_000_000)
        .context("Failed to create index writer")?;

    let mut report = run_sync(config, &sources, &mut writer, &fields, started)?;
    report.migrated = migrating;
    Ok(report)
}

/// One sync pass over `sources`. Caller must hold the writer lock.
/// Reloads and persists state so concurrent-safe with the fast path.
pub fn run_sync(
    _config: &Config,
    sources: &[(String, PathBuf)],
    writer: &mut tantivy::IndexWriter,
    fields: &LogFields,
    started: Instant,
) -> Result<SyncReport> {
    let base_dir = crate::config::base_dir();
    // Re-load under the lock (double-checked: another process may have
    // synced between our fast-path check and lock acquisition).
    let mut stat = SyncState::load(&base_dir);

    let mut ingested: u64 = 0;
    let mut failed: u64 = 0;
    let mut state_dirty = false;

    for (source, path) in sources {
        if !path.exists() {
            continue;
        }
        if is_fresh(path, stat.get(path)) {
            continue;
        }

        let prev = stat.get(path).map(|fs| FileCursor {
            offset: fs.offset,
            size: fs.size,
            mtime_ms: fs.mtime_ms,
            fingerprint: fs.fingerprint.clone(),
        });

        let start_offset = match &prev {
            Some(cursor) => match tail::detect_rotation(path, cursor) {
                Ok(true) => {
                    crate::log_info!(
                        "{} ({}) - rotation detected, re-reading from start",
                        source,
                        path.display()
                    );
                    0
                }
                Ok(false) => cursor.offset,
                Err(e) => {
                    crate::log_warn!("Failed to check rotation for {:?}: {}", path, e);
                    continue;
                }
            },
            None => 0,
        };

        match tail::ingest_new_lines(path, source, start_offset, writer, fields) {
            Ok((cursor, tail_stats)) => {
                ingested += tail_stats.indexed;
                failed += tail_stats.failed;
                stat.insert(
                    path,
                    FileState {
                        source: source.clone(),
                        offset: cursor.offset,
                        size: cursor.size,
                        mtime_ms: cursor.mtime_ms,
                        fingerprint: cursor.fingerprint,
                        last_ingested_at: Utc::now().to_rfc3339(),
                    },
                );
                state_dirty = true;
                if tail_stats.indexed > 0 {
                    crate::log_info!(
                        "{} ({}) - indexed: {}, failed: {}",
                        source,
                        path.display(),
                        tail_stats.indexed,
                        tail_stats.failed
                    );
                }
            }
            Err(e) => {
                crate::log_warn!("Failed to sync {:?}: {}", path, e);
                failed += 1;
            }
        }
    }

    if ingested > 0 {
        writer.commit().context("Failed to commit index")?;
    }
    if state_dirty {
        stat.save(&base_dir)?;
    }

    Ok(SyncReport {
        skipped: false,
        reason: None,
        files_checked: sources.len(),
        ingested,
        failed,
        elapsed_ms: elapsed_ms(started),
        migrated: false,
    })
}
