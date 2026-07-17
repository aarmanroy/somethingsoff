//! Index management commands

use anyhow::{Context, Result};
use chrono::{Duration, Utc};
use clap::{Args, Subcommand};
use std::io::{self, IsTerminal, Write};
use std::ops::Bound;

use tantivy::query::RangeQuery;
use tantivy::schema::Type;
use tantivy::Term;

use crate::config::Config;
use crate::index::builder::IndexBuilder;
use crate::index::searcher::format_timestamp;
use crate::output::{CliError, Envelope, ErrorCode};
use crate::schema::{create_schema, LogFields};

/// Manage the search index
#[derive(Args)]
pub struct IndexCommand {
    #[command(subcommand)]
    pub command: IndexSubcommand,
}

#[derive(Subcommand)]
pub enum IndexSubcommand {
    /// Rebuild index from scratch
    Rebuild,
    /// Show index status and stats
    Status,
    /// Clean old entries per retention policy
    Clean(CleanOptions),
}

/// Options for the clean command
#[derive(Args)]
pub struct CleanOptions {
    /// Dry run - show what would be deleted without actually deleting
    #[arg(long = "dry-run")]
    pub dry_run: bool,

    /// Force - skip confirmation prompt
    #[arg(long)]
    pub force: bool,
}

impl IndexCommand {
    pub async fn execute(&self) -> Result<u8> {
        match &self.command {
            IndexSubcommand::Rebuild => self.rebuild().await,
            IndexSubcommand::Status => self.status().await,
            IndexSubcommand::Clean(options) => self.clean(options).await,
        }
    }

    async fn rebuild(&self) -> Result<u8> {
        let envelope = Envelope::new("index");
        let config = Config::load()?;

        // Remove existing index; the builder recreates it from all sources.
        let index_dir = config.index_dir();
        if index_dir.exists() {
            std::fs::remove_dir_all(index_dir)
                .with_context(|| format!("Failed to remove index directory: {:?}", index_dir))?;
        }

        let builder = IndexBuilder::new(config);
        let stats = builder.build()?;

        envelope.emit(
            serde_json::json!({
                "action": "rebuild",
                "files_processed": stats.files_processed,
                "files_failed": stats.files_failed,
                "entries_indexed": stats.entries_indexed,
                "size_mb": stats.size_mb,
            }),
            None,
        )?;
        Ok(0)
    }

    async fn status(&self) -> Result<u8> {
        let envelope = Envelope::new("index");
        let config = Config::load()?;

        let (index, _fields) = crate::sync::open_or_create_index(&config)?;
        let reader = index.reader().context("Failed to open index reader")?;
        let documents = reader.searcher().num_docs();

        let index_dir = config.index_dir();
        let size = get_dir_size(index_dir);
        let size_mb = (size as f64 / (1024.0 * 1024.0) * 100.0).round() / 100.0;

        envelope.emit(
            serde_json::json!({
                "action": "status",
                "documents": documents,
                "size_mb": size_mb,
                "path": index_dir.display().to_string(),
            }),
            None,
        )?;
        Ok(0)
    }

    async fn clean(&self, options: &CleanOptions) -> Result<u8> {
        let envelope = Envelope::new("index");
        let config = Config::load()?;
        let index_dir = config.index_dir();

        if !index_dir.exists() {
            // Nothing to clean on a fresh project.
            envelope.emit(
                serde_json::json!({
                    "action": "clean",
                    "deleted": 0,
                    "message": "No index exists yet; nothing to clean.",
                }),
                None,
            )?;
            return Ok(0);
        }

        // Calculate cutoff timestamp (entries older than this will be deleted)
        let retention_days = config.general.retention_days;
        let cutoff = Utc::now() - Duration::days(retention_days as i64);
        let cutoff_timestamp = format_timestamp(&cutoff);

        crate::log_info!(
            "Finding entries older than {} (retention: {} days)",
            cutoff_timestamp,
            retention_days
        );

        let schema = create_schema();
        let fields = LogFields::new(&schema)?;
        let (index, _) = crate::sync::open_or_create_index(&config)?;

        let reader = index
            .reader_builder()
            .reload_policy(tantivy::ReloadPolicy::OnCommitWithDelay)
            .try_into()
            .context("Failed to create index reader")?;

        // Build range query to find old entries (timestamp < cutoff)
        let field_name = "timestamp".to_string();
        let upper_bound =
            Bound::Included(Term::from_field_text(fields.timestamp, &cutoff_timestamp));
        let range_query =
            RangeQuery::new_term_bounds(field_name, Type::Str, &Bound::Unbounded, &upper_bound);

        let searcher = reader.searcher();
        let entries_to_delete = searcher
            .search(&range_query, &tantivy::collector::Count)
            .context("Failed to count old entries")?;

        if entries_to_delete == 0 {
            let journals_pruned = if options.dry_run {
                0
            } else {
                prune_old_journals(retention_days)
            };
            envelope.emit(
                serde_json::json!({
                    "action": "clean",
                    "deleted": 0,
                    "journals_pruned": journals_pruned,
                    "retention_days": retention_days,
                    "cutoff": cutoff_timestamp,
                    "message": "No entries older than the retention period.",
                }),
                None,
            )?;
            return Ok(0);
        }

        if options.dry_run {
            envelope.emit(
                serde_json::json!({
                    "action": "clean",
                    "dry_run": true,
                    "would_delete": entries_to_delete,
                    "retention_days": retention_days,
                    "cutoff": cutoff_timestamp,
                }),
                None,
            )?;
            return Ok(0);
        }

        // Confirmation: prompt only on an interactive terminal. Non-TTY
        // callers (agents, scripts) must pass --force — never hang.
        if !options.force {
            if !io::stdin().is_terminal() {
                return Err(CliError::new(
                    ErrorCode::Usage,
                    format!(
                        "Refusing to delete {} entries without confirmation in a non-interactive session",
                        entries_to_delete
                    ),
                )
                .with_hint("Re-run with --force to confirm, or --dry-run to preview.")
                .into());
            }
            let confirmed = confirm_action(&format!(
                "Delete {} entries older than {} days? [y/N]: ",
                entries_to_delete, retention_days
            ))?;
            if !confirmed {
                envelope.emit(
                    serde_json::json!({
                        "action": "clean",
                        "cancelled": true,
                        "message": "Operation cancelled by user.",
                    }),
                    None,
                )?;
                return Ok(0);
            }
        }

        // Perform actual deletion
        crate::log_info!("Deleting {} old entries...", entries_to_delete);
        let deleted_count =
            self.delete_old_entries(&index, &range_query, entries_to_delete as u64)?;
        crate::log_info!("Deleted {} entries", deleted_count);

        let journals_pruned = prune_old_journals(retention_days);

        let size = get_dir_size(index_dir);
        let size_mb = (size as f64 / (1024.0 * 1024.0) * 100.0).round() / 100.0;

        envelope.emit(
            serde_json::json!({
                "action": "clean",
                "deleted": deleted_count,
                "journals_pruned": journals_pruned,
                "retention_days": retention_days,
                "cutoff": cutoff_timestamp,
                "size_mb": size_mb,
            }),
            None,
        )?;
        Ok(0)
    }

    /// Delete old entries using Tantivy delete queries
    fn delete_old_entries(
        &self,
        index: &tantivy::Index,
        query: &RangeQuery,
        expected_count: u64,
    ) -> Result<u64> {
        let mut writer: tantivy::IndexWriter<tantivy::TantivyDocument> = index
            .writer_with_num_threads(1, 50_000_000)
            .context("Failed to create index writer")?;

        writer
            .delete_query(Box::new(query.clone()))
            .context("Failed to execute delete query")?;

        writer.commit().context("Failed to commit deletions")?;
        writer
            .wait_merging_threads()
            .context("Failed to wait for merge")?;

        Ok(expected_count)
    }
}

/// Delete tap journals not modified within the retention window.
/// Returns the number of journal files removed.
fn prune_old_journals(retention_days: u32) -> usize {
    let streams_dir = crate::config::base_dir().join(crate::sync::discover::STREAMS_DIR);
    let cutoff = std::time::SystemTime::now()
        - std::time::Duration::from_secs(retention_days as u64 * 24 * 3600);
    let mut pruned = 0;
    if let Ok(entries) = std::fs::read_dir(&streams_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            let is_old = entry
                .metadata()
                .and_then(|m| m.modified())
                .map(|mtime| mtime < cutoff)
                .unwrap_or(false);
            if path.is_file() && is_old && std::fs::remove_file(&path).is_ok() {
                crate::log_info!("Pruned old journal: {}", path.display());
                pruned += 1;
            }
        }
    }
    pruned
}

/// Prompt user for confirmation (interactive terminals only)
fn confirm_action(prompt: &str) -> Result<bool> {
    print!("{}", prompt);
    io::stdout().flush().context("Failed to flush stdout")?;

    let mut input = String::new();
    io::stdin()
        .read_line(&mut input)
        .context("Failed to read input")?;

    Ok(input.trim().to_lowercase() == "y" || input.trim().to_lowercase() == "yes")
}

fn get_dir_size(path: &std::path::Path) -> u64 {
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

    #[test]
    fn test_clean_options_defaults() {
        let options = CleanOptions {
            dry_run: false,
            force: false,
        };
        assert!(!options.dry_run);
        assert!(!options.force);
    }

    #[test]
    fn test_clean_options_flags() {
        let options = CleanOptions {
            dry_run: true,
            force: true,
        };
        assert!(options.dry_run);
        assert!(options.force);
    }

    #[test]
    fn test_get_dir_size() {
        use tempfile::TempDir;

        let temp_dir = TempDir::new().expect("Failed to create temp dir");
        let size = get_dir_size(temp_dir.path());
        assert_eq!(size, 0, "Empty directory should have size 0");
    }

    #[test]
    fn test_cutoff_timestamp_calculation() {
        let retention_days: u32 = 30;
        let cutoff = Utc::now() - Duration::days(retention_days as i64);
        let cutoff_timestamp = format_timestamp(&cutoff);

        assert!(cutoff_timestamp.contains('T'));
        assert!(cutoff_timestamp.ends_with('Z'));
        assert!(cutoff_timestamp.contains('.'));

        let now = Utc::now();
        let diff = now - cutoff;
        assert_eq!(diff.num_days(), 30);
    }
}
