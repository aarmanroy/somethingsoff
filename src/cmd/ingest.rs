//! Ingest command implementation
//!
//! One-shot ingestion of a single log file. Idempotent: deduplication via
//! log_id upserts makes re-ingesting the same file safe. The file is also
//! registered in the sync state, so subsequent read commands keep it fresh
//! automatically (appends are picked up without running `ingest` again).

use anyhow::{Context, Result};
use chrono::Utc;
use clap::Args;
use std::path::PathBuf;
use std::time::Duration;

use crate::config::Config;
use crate::index::upsert::{count_docs, reader_for};
use crate::output::{CliError, Envelope, ErrorCode};
use crate::sync::lock::SyncLock;
use crate::sync::state::{FileState, SyncState};
use crate::sync::{open_or_create_index, tail};

const LOCK_TIMEOUT: Duration = Duration::from_secs(5);

/// Ingest a log file into the index
#[derive(Args)]
pub struct IngestCommand {
    /// Source name for this log file (e.g., backend, frontend, telemetry)
    #[arg(short, long)]
    pub source: String,

    /// Path to the log file to ingest
    #[arg(short, long)]
    pub file: PathBuf,
}

impl IngestCommand {
    pub async fn execute(&self) -> Result<u8> {
        let envelope = Envelope::new("ingest");
        let config = Config::load()?;

        // Validate source name (normalize to lowercase)
        let source = self.source.to_lowercase();

        // Validate file exists
        if !self.file.exists() {
            return Err(CliError::new(
                ErrorCode::Usage,
                format!("File not found: {}", self.file.display()),
            )
            .with_hint("Check the --file path. Files in ./logs/ are ingested automatically — no explicit ingest needed.")
            .into());
        }

        let base_dir = crate::config::base_dir();
        let _lock = SyncLock::acquire_blocking(&base_dir, LOCK_TIMEOUT)?;
        if crate::sync::needs_migration(&config) {
            crate::sync::migrate_index(&config)?;
        }

        let (index, fields) = open_or_create_index(&config)?;
        let mut writer = index
            .writer_with_num_threads(1, 50_000_000)
            .context("Failed to create index writer")?;

        // Snapshot doc count before ingestion for accurate dedup calculation
        let reader = reader_for(&index)?;
        let docs_before = count_docs(&reader);

        // Full-file ingest from offset 0 (dedup absorbs any overlap with
        // previous ingests or auto-sync passes).
        crate::log_info!("Ingesting {} logs from: {:?}", source, self.file);
        let (cursor, stats) = tail::ingest_new_lines(&self.file, &source, 0, &mut writer, &fields)?;

        // Commit changes
        writer.commit().context("Failed to commit index")?;
        writer
            .wait_merging_threads()
            .context("Failed to wait for merge")?;

        // Calculate accurate dedup count: how many entries replaced existing docs?
        reader.reload().context("Failed to reload reader")?;
        let docs_after = count_docs(&reader);
        let entries_deduplicated = stats.indexed.saturating_sub(docs_after - docs_before);

        // Register the file so auto-sync keeps it fresh from here on.
        let mut sync_state = SyncState::load(&base_dir);
        sync_state.insert(
            &self.file,
            FileState {
                source: source.clone(),
                offset: cursor.offset,
                size: cursor.size,
                mtime_ms: cursor.mtime_ms,
                fingerprint: cursor.fingerprint,
                last_ingested_at: Utc::now().to_rfc3339(),
            },
        );
        sync_state.save(&base_dir)?;

        crate::log_info!(
            "Ingestion complete: {} entries indexed, {} deduplicated, {} failed",
            stats.indexed,
            entries_deduplicated,
            stats.failed,
        );

        envelope.emit(
            serde_json::json!({
                "source": source,
                "file": self.file.display().to_string(),
                "entries_indexed": stats.indexed,
                "entries_deduplicated": entries_deduplicated,
                "entries_failed": stats.failed,
            }),
            None,
        )?;
        Ok(0)
    }
}
