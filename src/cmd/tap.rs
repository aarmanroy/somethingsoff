//! Tap command - pipe any process's output through somethingsoff.
//!
//! `npm run dev 2>&1 | somethingsoff tap` makes log capture universal: no
//! SDK, no file management, any language. Every line is:
//!   1. echoed verbatim to stdout (your terminal still shows the logs,
//!      colors and all — ANSI is stripped only on the indexed copy),
//!   2. appended to a journal at `<base_dir>/streams/<source>.jsonl`
//!      (so the data survives rebuilds and lock contention), and
//!   3. best-effort ingested inline when the writer lock is available.
//!
//! Ingest is lossless: structured lines are parsed into fields, and any
//! plain-text line (dev-server chatter, build/test output) is captured as a
//! raw entry with a sniffed level — so unstructured output is searchable too.
//!
//! If another writer (e.g. `watch`) holds the lock, tap runs journal-only —
//! the journal is a discovered source, so the next sync indexes it anyway.
//!
//! The summary envelope goes to STDERR: stdout belongs to the passthrough.

use anyhow::{Context, Result};
use chrono::Utc;
use clap::Args;
use std::io::Write;
use std::time::{Duration, Instant};

use crate::config::Config;
use crate::output::Envelope;
use crate::parser::coalesce::Block;
use crate::parser::{parse_block, LineCoalescer};
use crate::schema::{LogEntry, LogFields};
use crate::sync::discover::STREAMS_DIR;
use crate::sync::lock::SyncLock;
use crate::sync::state::{FileState, SyncState};
use crate::sync::{open_or_create_index, tail};

const COMMIT_EVERY_ENTRIES: u64 = 500;
const COMMIT_EVERY: Duration = Duration::from_secs(1);

/// Pipe stdin through: echo + journal + index (`app | somethingsoff tap`)
#[derive(Args)]
pub struct TapCommand {
    /// Source name for the piped stream
    #[arg(short, long, default_value = "stdin")]
    pub source: String,
}

struct InlineIndexer {
    _lock: SyncLock,
    writer: tantivy::IndexWriter,
    fields: LogFields,
    entries_since_commit: u64,
    last_commit: Instant,
}

impl TapCommand {
    pub async fn execute(&self) -> Result<u8> {
        let envelope = Envelope::new("tap");
        let config = Config::load()?;
        let source = self.source.to_lowercase();
        let base_dir = crate::config::base_dir();

        // Journal setup (append mode; previous content is already tracked
        // in sync state or will be picked up by discovery).
        let streams_dir = base_dir.join(STREAMS_DIR);
        std::fs::create_dir_all(&streams_dir)
            .with_context(|| format!("Failed to create streams dir: {:?}", streams_dir))?;
        let journal_path = streams_dir.join(format!("{}.jsonl", source));
        let mut journal = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&journal_path)
            .with_context(|| format!("Failed to open journal: {:?}", journal_path))?;
        let mut journal_offset = journal.metadata().map(|m| m.len()).unwrap_or(0);

        // Best-effort inline indexing: only if the writer lock is free.
        let mut indexer = match SyncLock::try_acquire(&base_dir)? {
            Some(lock) => {
                if crate::sync::needs_migration(&config) {
                    crate::sync::migrate_index(&config)?;
                }
                let (index, fields) = open_or_create_index(&config)?;
                let writer = index
                    .writer_with_num_threads(1, 50_000_000)
                    .context("Failed to create index writer")?;
                Some(InlineIndexer {
                    _lock: lock,
                    writer,
                    fields,
                    entries_since_commit: 0,
                    last_commit: Instant::now(),
                })
            }
            None => {
                crate::log_info!(
                    "Another writer holds the lock; journaling only (data is indexed on the next sync)"
                );
                None
            }
        };

        // Read stdin on a blocking thread; select against Ctrl+C so the
        // final commit and cursor update always run.
        let (line_tx, mut line_rx) = tokio::sync::mpsc::channel::<String>(1024);
        std::thread::spawn(move || {
            use std::io::BufRead;
            let stdin = std::io::stdin();
            for line in stdin.lock().lines() {
                match line {
                    Ok(line) => {
                        if line_tx.blocking_send(line).is_err() {
                            break; // receiver gone (shutdown)
                        }
                    }
                    Err(_) => break,
                }
            }
            // Channel drops here → EOF signal to the main loop.
        });

        let mut stdout = std::io::stdout();
        let mut lines_seen: u64 = 0;
        let mut indexed: u64 = 0;
        let mut failed: u64 = 0;
        // Fold lines into events (stack traces, diagnostic blocks). A block
        // is indexed when the next event's first line arrives or at EOF —
        // one line of latency, never partial events.
        let mut coalescer = LineCoalescer::new();

        loop {
            let line = tokio::select! {
                maybe_line = line_rx.recv() => match maybe_line {
                    Some(line) => line,
                    None => break, // EOF
                },
                _ = tokio::signal::ctrl_c() => break,
            };

            lines_seen += 1;

            // 1. Passthrough (flush per line: dev servers expect immediacy)
            writeln!(stdout, "{}", line).ok();
            stdout.flush().ok();

            // 2. Journal
            let line_start = journal_offset;
            writeln!(journal, "{}", line).context("Failed to write journal")?;
            journal_offset += line.len() as u64 + 1;

            // 3. Inline ingest (journal-only mode leaves indexing to sync,
            // which coalesces identically when replaying the journal)
            if let Some(ref mut idx) = indexer {
                if let Some(block) = coalescer.push(&line, line_start) {
                    index_tap_block(
                        &block,
                        &journal_path,
                        &source,
                        idx,
                        &mut indexed,
                        &mut failed,
                    );
                }

                if idx.entries_since_commit >= COMMIT_EVERY_ENTRIES
                    || (idx.entries_since_commit > 0 && idx.last_commit.elapsed() >= COMMIT_EVERY)
                {
                    idx.writer.commit().context("Failed to commit index")?;
                    idx.entries_since_commit = 0;
                    idx.last_commit = Instant::now();
                    // Never persist the cursor past a buffered (unindexed)
                    // block: on crash, sync re-reads it; position-seeded ids
                    // dedup anything already indexed.
                    let safe_offset = coalescer.pending_start().unwrap_or(journal_offset);
                    save_cursor(&base_dir, &journal_path, &source, safe_offset)?;
                }
            }
        }

        // Final flush: index the in-progress event, commit, and record the
        // journal cursor so future syncs only read appends.
        journal.flush().ok();
        if let Some(ref mut idx) = indexer {
            if let Some(block) = coalescer.flush() {
                index_tap_block(
                    &block,
                    &journal_path,
                    &source,
                    idx,
                    &mut indexed,
                    &mut failed,
                );
            }
            if idx.entries_since_commit > 0 {
                idx.writer.commit().context("Failed to commit index")?;
            }
            save_cursor(&base_dir, &journal_path, &source, journal_offset)?;
        }

        // Summary on stderr — stdout is reserved for the passthrough.
        let summary = envelope.render(
            serde_json::json!({
                "source": source,
                "journal": journal_path.display().to_string(),
                "lines": lines_seen,
                "entries_indexed": indexed,
                "entries_failed": failed,
                "indexed_inline": indexer.is_some(),
            }),
            None,
        )?;
        eprintln!("{}", summary);

        Ok(0)
    }
}

/// Parse one coalesced event and upsert it inline. Position = journal byte
/// offset, identical to what sync computes replaying the journal — so inline
/// and replay paths produce the same log_ids (no duplicates either way).
fn index_tap_block(
    block: &Block,
    journal_path: &std::path::Path,
    source: &str,
    idx: &mut InlineIndexer,
    indexed: &mut u64,
    failed: &mut u64,
) {
    // A None from parse_block is a decoration-only event: not a failure.
    if let Some(mut raw) = parse_block(&block.text, source) {
        raw.ingest_position = Some(format!("{}:{}", journal_path.display(), block.first_byte));
        let raw = crate::pii::redact_raw_entry(raw);
        let entry = LogEntry::from_raw(raw, source);
        if crate::index::upsert::upsert_entry(&mut idx.writer, &idx.fields, &entry, false).is_ok() {
            *indexed += 1;
            idx.entries_since_commit += 1;
        } else {
            *failed += 1;
        }
    }
}

/// Persist the journal read-cursor at `offset` (bytes already indexed).
fn save_cursor(
    base_dir: &std::path::Path,
    journal_path: &std::path::Path,
    source: &str,
    offset: u64,
) -> Result<()> {
    let (size, mtime_ms) = tail::file_meta(journal_path)?;
    let mut state = SyncState::load(base_dir);
    state.insert(
        journal_path,
        FileState {
            source: source.to_string(),
            offset,
            size,
            mtime_ms,
            fingerprint: tail::head_fingerprint(journal_path)?,
            last_ingested_at: Utc::now().to_rfc3339(),
        },
    );
    state.save(base_dir)
}
