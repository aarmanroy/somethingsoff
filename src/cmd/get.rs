//! Get command - retrieve logs by specific IDs

use anyhow::Result;
use clap::Args;

use crate::config::Config;
use crate::index::searcher::SearchOptions;
use crate::output::{list_meta, CliError, Envelope, ErrorCode};

/// Get logs by a specific ID (log_id hash, request, or user)
#[derive(Args)]
pub struct GetCommand {
    /// Get a specific log by its log_id (hash)
    #[arg(value_name = "LOG_ID")]
    pub log_id: Option<String>,

    /// Get all logs for a request ID
    #[arg(short = 'R', long = "request-id")]
    pub request_id: Option<String>,

    /// Get all logs for a user ID
    #[arg(short = 'U', long = "user-id")]
    pub user_id: Option<String>,

    /// Compact output - omit null fields
    #[arg(long)]
    pub compact: bool,

    /// Field selection - only return specified fields (comma-separated)
    #[arg(long, value_delimiter = ',')]
    pub fields: Option<Vec<String>>,
}

impl GetCommand {
    pub async fn execute(&self) -> Result<u8> {
        // Exactly one selector: `get` is a point lookup, not a dump.
        let selectors = [
            self.log_id.is_some(),
            self.request_id.is_some(),
            self.user_id.is_some(),
        ]
        .iter()
        .filter(|s| **s)
        .count();
        if selectors != 1 {
            return Err(CliError::new(
                ErrorCode::Usage,
                "get requires exactly one selector",
            )
            .with_hint(
                "Pass a LOG_ID argument, --request-id <ID>, or --user-id <ID>. To list logs, use `somethingsoff search`.",
            )
            .into());
        }

        let mut envelope = Envelope::new("get");
        let config = Config::load()?;
        let (searcher, sync_report) = crate::cmd::prepare_read(config)?;
        envelope.set_sync(sync_report);

        let options = SearchOptions {
            log_id: self.log_id.clone(),
            request_id: self.request_id.clone(),
            user_id: self.user_id.clone(),
            limit: 10000,
            ..Default::default()
        };

        let output = searcher.search(options)?;
        let results_json = crate::schema::format_log_entries(
            &output.results,
            self.compact,
            self.fields.as_deref(),
        )?;
        let count = output.results.len();

        if crate::is_jsonl() {
            if let serde_json::Value::Array(entries) = results_json {
                envelope.emit_jsonl(&entries)?;
            }
        } else {
            let meta = list_meta(
                count,
                output.total,
                None,
                None,
                serde_json::Value::Object(output.filters),
                output.time_range,
            );
            envelope.emit(serde_json::json!({ "results": results_json }), Some(meta))?;
        }

        Ok(if count == 0 { 2 } else { 0 })
    }
}
