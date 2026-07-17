//! Incremental file tailing: read only new bytes since the last cursor,
//! with rotation detection via size shrink or head-fingerprint change.
//!
//! This is the single read→parse→redact→upsert loop shared by auto-sync,
//! `watch`, `ingest`, and index rebuild (it replaces the three near-identical
//! copies that previously lived in serve.rs, ingest.rs, and builder.rs).

use anyhow::{Context, Result};
use sha2::{Digest, Sha256};
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::Path;
use tantivy::IndexWriter;

use crate::index::upsert::upsert_entry;
use crate::parser::coalesce::Block;
use crate::parser::{parse_block, LineCoalescer};
use crate::schema::{LogEntry, LogFields};

/// How many leading bytes participate in the head fingerprint.
const FINGERPRINT_HEAD_BYTES: usize = 4096;

/// Commit mid-ingest every this many indexed documents. Without this, a single
/// large file buffers every document *and* every pending dedup delete in the
/// writer until the one end-of-pass commit — ~1.4GB of resident memory for a
/// 1M-line file, growing linearly (a 10M-line file would risk OOM). Periodic
/// commits flush that buffer; re-reading after a crash is still safe because
/// dedup collapses any re-ingested lines.
const COMMIT_EVERY: u64 = 200_000;

/// Point-in-time read cursor for one file.
#[derive(Debug, Clone, Default)]
pub struct FileCursor {
    /// Byte offset of the next unread byte
    pub offset: u64,
    /// File size when the cursor was taken
    pub size: u64,
    /// mtime in ms since epoch when the cursor was taken
    pub mtime_ms: Option<u64>,
    /// Fingerprint of the first `min(4096, len)` bytes
    pub fingerprint: String,
}

/// Outcome of one incremental ingest pass over a file.
#[derive(Debug, Clone, Default)]
pub struct TailStats {
    pub indexed: u64,
    pub failed: u64,
    pub rotated: bool,
}

/// Stat a file: (size, mtime in ms since epoch).
pub fn file_meta(path: &Path) -> Result<(u64, Option<u64>)> {
    let metadata =
        std::fs::metadata(path).with_context(|| format!("Failed to stat log file: {:?}", path))?;
    let mtime_ms = metadata
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as u64);
    Ok((metadata.len(), mtime_ms))
}

/// Hash the first `min(4096, len)` bytes. An empty file fingerprints to "".
pub fn head_fingerprint(path: &Path) -> Result<String> {
    let mut file =
        std::fs::File::open(path).with_context(|| format!("Failed to open: {:?}", path))?;
    let mut buf = vec![0u8; FINGERPRINT_HEAD_BYTES];
    let mut read_total = 0;
    while read_total < FINGERPRINT_HEAD_BYTES {
        let n = file.read(&mut buf[read_total..])?;
        if n == 0 {
            break;
        }
        read_total += n;
    }
    if read_total == 0 {
        return Ok(String::new());
    }
    let hash = Sha256::digest(&buf[..read_total]);
    Ok(hex::encode(hash)[..16].to_string())
}

/// A file was rotated/replaced if it shrank or its head content changed.
/// (More reliable than the previous mtime-jump heuristic: `copytruncate`
/// rotation shrinks the file; move-and-recreate changes the head bytes.)
pub fn detect_rotation(path: &Path, prev: &FileCursor) -> Result<bool> {
    let (size, _) = file_meta(path)?;
    if size < prev.size {
        return Ok(true);
    }
    // A previously-empty file gaining content is growth, not rotation.
    if prev.fingerprint.is_empty() {
        return Ok(false);
    }
    Ok(head_fingerprint(path)? != prev.fingerprint)
}

/// Ingest all complete-or-final lines from `start_offset` to EOF.
/// Returns the updated cursor (offset = bytes consumed) and stats.
///
/// Lines are read as raw bytes (`read_until`) so a stray non-UTF-8 line is
/// lossily decoded instead of aborting the whole pass, and the byte offset
/// stays exact regardless of encoding.
pub fn ingest_new_lines(
    path: &Path,
    source: &str,
    start_offset: u64,
    writer: &mut IndexWriter,
    fields: &LogFields,
) -> Result<(FileCursor, TailStats)> {
    let file = std::fs::File::open(path).with_context(|| format!("Failed to open: {:?}", path))?;
    let mut reader = BufReader::new(file);
    reader
        .seek(SeekFrom::Start(start_offset))
        .with_context(|| format!("Failed to seek to {} in {:?}", start_offset, path))?;

    let mut stats = TailStats::default();
    let mut offset = start_offset;
    let mut buf: Vec<u8> = Vec::new();
    // Fold physical lines into events: continuation lines (stack frames,
    // cargo code frames) glue to their anchor so one failure = one entry.
    let mut coalescer = LineCoalescer::new();

    loop {
        buf.clear();
        let line_start = offset;
        let n = reader
            .read_until(b'\n', &mut buf)
            .with_context(|| format!("Failed to read from {:?}", path))?;
        if n == 0 {
            break;
        }
        offset += n as u64;

        let line = String::from_utf8_lossy(&buf);
        let line = line.trim_end_matches(['\n', '\r']);
        if let Some(block) = coalescer.push(line, line_start) {
            index_block(&block, path, source, writer, fields, &mut stats);
        }
    }
    // EOF terminates the final in-progress event.
    if let Some(block) = coalescer.flush() {
        index_block(&block, path, source, writer, fields, &mut stats);
    }

    let (size, mtime_ms) = file_meta(path)?;
    let cursor = FileCursor {
        offset,
        size,
        mtime_ms,
        fingerprint: head_fingerprint(path)?,
    };
    Ok((cursor, stats))
}

/// Parse one coalesced event and upsert it. A `None` from `parse_block` is a
/// decoration-only block (ANSI, box-drawing): nothing to index, not a failure.
fn index_block(
    block: &Block,
    path: &Path,
    source: &str,
    writer: &mut IndexWriter,
    fields: &LogFields,
    stats: &mut TailStats,
) {
    if let Some(mut raw) = parse_block(&block.text, source) {
        // Stable position: salts log_id for entries with no intrinsic
        // timestamp (honest counts + replay-safe dedup, see schema v6).
        raw.ingest_position = Some(format!("{}:{}", path.display(), block.first_byte));
        let raw = crate::pii::redact_raw_entry(raw);
        let entry = LogEntry::from_raw(raw, source);
        if let Err(e) = upsert_entry(writer, fields, &entry, false) {
            crate::log_warn!(
                "Failed to index block at byte {} in {:?}: {}",
                block.first_byte,
                path,
                e
            );
            stats.failed += 1;
        } else {
            stats.indexed += 1;
            // Flush the writer's document+delete buffer periodically to keep
            // memory flat on large files (see COMMIT_EVERY).
            if stats.indexed.is_multiple_of(COMMIT_EVERY) {
                if let Err(e) = writer.commit() {
                    crate::log_warn!("Periodic commit failed in {:?}: {}", path, e);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::create_schema;
    use std::io::Write;
    use tantivy::Index;
    use tempfile::TempDir;

    fn test_writer() -> (Index, IndexWriter, LogFields) {
        let schema = create_schema();
        let fields = LogFields::new(&schema).unwrap();
        let index = Index::create_in_ram(schema);
        let writer = index.writer_with_num_threads(1, 50_000_000).unwrap();
        (index, writer, fields)
    }

    fn write_lines(path: &Path, lines: &[&str]) {
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .unwrap();
        for line in lines {
            writeln!(f, "{}", line).unwrap();
        }
    }

    #[test]
    fn test_ingest_from_zero_and_incremental_append() {
        let dir = TempDir::new().unwrap();
        let log = dir.path().join("app.log");
        write_lines(
            &log,
            &[r#"{"timestamp":"2026-07-15T10:00:00.000Z","level":"info","message":"one"}"#],
        );

        let (_index, mut writer, fields) = test_writer();
        let (cursor, stats) = ingest_new_lines(&log, "app", 0, &mut writer, &fields).unwrap();
        assert_eq!(stats.indexed, 1);
        assert_eq!(stats.failed, 0);
        assert_eq!(cursor.offset, cursor.size);

        // Append one more line; a second pass from the cursor sees only it.
        write_lines(
            &log,
            &[r#"{"timestamp":"2026-07-15T10:00:01.000Z","level":"error","message":"two"}"#],
        );
        let (cursor2, stats2) =
            ingest_new_lines(&log, "app", cursor.offset, &mut writer, &fields).unwrap();
        assert_eq!(stats2.indexed, 1);
        assert!(cursor2.offset > cursor.offset);
    }

    #[test]
    fn test_empty_and_blank_lines_skipped() {
        let dir = TempDir::new().unwrap();
        let log = dir.path().join("app.log");
        std::fs::write(&log, "\n   \n").unwrap();

        let (_index, mut writer, fields) = test_writer();
        let (cursor, stats) = ingest_new_lines(&log, "app", 0, &mut writer, &fields).unwrap();
        assert_eq!(stats.indexed, 0);
        assert_eq!(stats.failed, 0);
        // Offset still advances past blank lines so they are not re-read.
        assert_eq!(cursor.offset, cursor.size);
    }

    #[test]
    fn test_final_line_without_newline_is_ingested() {
        let dir = TempDir::new().unwrap();
        let log = dir.path().join("app.log");
        std::fs::write(
            &log,
            r#"{"timestamp":"2026-07-15T10:00:00.000Z","level":"info","message":"tail"}"#,
        )
        .unwrap();

        let (_index, mut writer, fields) = test_writer();
        let (cursor, stats) = ingest_new_lines(&log, "app", 0, &mut writer, &fields).unwrap();
        assert_eq!(stats.indexed, 1);
        assert_eq!(cursor.offset, cursor.size);
    }

    #[test]
    fn test_detect_rotation_on_shrink() {
        let dir = TempDir::new().unwrap();
        let log = dir.path().join("app.log");
        std::fs::write(&log, "a long first line for the fingerprint\n").unwrap();

        let (size, mtime_ms) = file_meta(&log).unwrap();
        let prev = FileCursor {
            offset: size,
            size,
            mtime_ms,
            fingerprint: head_fingerprint(&log).unwrap(),
        };

        // Same content: no rotation.
        assert!(!detect_rotation(&log, &prev).unwrap());

        // Truncate + rewrite with different content: rotation.
        std::fs::write(&log, "new\n").unwrap();
        assert!(detect_rotation(&log, &prev).unwrap());
    }

    #[test]
    fn test_detect_rotation_on_replaced_content_same_size() {
        let dir = TempDir::new().unwrap();
        let log = dir.path().join("app.log");
        std::fs::write(&log, "AAAAAAAAAA\n").unwrap();
        let prev = FileCursor {
            offset: 11,
            size: 11,
            mtime_ms: None,
            fingerprint: head_fingerprint(&log).unwrap(),
        };

        std::fs::write(&log, "BBBBBBBBBB\n").unwrap();
        assert!(detect_rotation(&log, &prev).unwrap());
    }

    #[test]
    fn test_previously_empty_file_growing_is_not_rotation() {
        let dir = TempDir::new().unwrap();
        let log = dir.path().join("app.log");
        std::fs::write(&log, "").unwrap();
        let prev = FileCursor {
            offset: 0,
            size: 0,
            mtime_ms: None,
            fingerprint: head_fingerprint(&log).unwrap(),
        };
        assert!(prev.fingerprint.is_empty());

        std::fs::write(&log, "first line\n").unwrap();
        assert!(!detect_rotation(&log, &prev).unwrap());
    }

    #[test]
    fn test_multiline_diagnostic_coalesces_to_one_entry() {
        let dir = TempDir::new().unwrap();
        let log = dir.path().join("build.log");
        write_lines(
            &log,
            &[
                "error[E0308]: mismatched types",
                "  --> src/lib.rs:7:5",
                "   |",
                " 7 |     \"oops\"",
                "   |     ^^^^^^ expected `i32`, found `&str`",
                "Compiling foo v0.1.0",
            ],
        );

        let (index, mut writer, fields) = test_writer();
        let (_cursor, stats) = ingest_new_lines(&log, "build", 0, &mut writer, &fields).unwrap();
        writer.commit().unwrap();

        // One diagnostic block + one plain line — not six fragments.
        assert_eq!(stats.indexed, 2);
        assert_eq!(index.reader().unwrap().searcher().num_docs(), 2);
    }

    #[test]
    fn test_identical_raw_lines_keep_counts_and_replays_dedup() {
        let dir = TempDir::new().unwrap();
        let log = dir.path().join("app.log");
        write_lines(
            &log,
            &[
                "retrying connection",
                "retrying connection",
                "retrying connection",
            ],
        );

        let (index, mut writer, fields) = test_writer();
        let (_c, stats) = ingest_new_lines(&log, "app", 0, &mut writer, &fields).unwrap();
        writer.commit().unwrap();
        // Position-seeded ids: three occurrences = three documents.
        assert_eq!(stats.indexed, 3);
        let reader = index.reader().unwrap();
        assert_eq!(reader.searcher().num_docs(), 3);

        // Full re-read (cursor loss) re-indexes the same positions: same
        // ids, so dedup holds — still exactly three documents.
        let (_c2, stats2) = ingest_new_lines(&log, "app", 0, &mut writer, &fields).unwrap();
        writer.commit().unwrap();
        assert_eq!(stats2.indexed, 3);
        reader.reload().unwrap();
        assert_eq!(reader.searcher().num_docs(), 3);
    }

    #[test]
    fn test_non_utf8_line_does_not_abort_pass() {
        let dir = TempDir::new().unwrap();
        let log = dir.path().join("app.log");
        let mut content = Vec::new();
        content.extend_from_slice(&[0xff, 0xfe, b'\n']);
        content.extend_from_slice(
            br#"{"timestamp":"2026-07-15T10:00:00.000Z","level":"info","message":"ok"}"#,
        );
        content.push(b'\n');
        std::fs::write(&log, content).unwrap();

        let (_index, mut writer, fields) = test_writer();
        let (_cursor, stats) = ingest_new_lines(&log, "app", 0, &mut writer, &fields).unwrap();
        assert_eq!(stats.indexed, 1);
        // The lossy-decoded garbage line has no alphanumeric content, so it is
        // skipped (not indexed, not a failure); the valid JSON line indexes.
        assert_eq!(stats.failed, 0);
    }
}
