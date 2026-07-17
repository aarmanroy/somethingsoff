//! Search command implementation

use anyhow::Result;
use clap::Args;

use crate::config::Config;
use crate::index::searcher::{SearchOptions, SortOrder};
use crate::output::{list_meta, CliError, Envelope, ErrorCode};

/// Search logs with filters
#[derive(Args)]
pub struct SearchCommand {
    /// Full-text search query
    #[arg(short, long)]
    pub query: Option<String>,

    /// Filter by log level (debug, info, warn, error)
    #[arg(short = 'L', long)]
    pub level: Option<String>,

    /// Filter by request ID
    #[arg(short = 'R', long = "request-id")]
    pub request_id: Option<String>,

    /// Filter by user ID
    #[arg(short = 'U', long = "user-id")]
    pub user_id: Option<String>,

    /// Filter by route
    #[arg(short = 'r', long)]
    pub route: Option<String>,

    /// Filter by source name (e.g. backend, api)
    #[arg(long)]
    pub source: Option<String>,

    /// Filter by HTTP method (GET, POST, ...)
    #[arg(long)]
    pub method: Option<String>,

    /// Filter by parse origin: "raw" (unparsed fallback), "json", "logfmt",
    /// "syslog", ... (see `stats --by-format` for what's in the index)
    #[arg(long = "parse-format", value_name = "FORMAT")]
    pub parse_format: Option<String>,

    /// Filter by status code: exact (500) or inclusive range (500-599)
    #[arg(long)]
    pub status: Option<String>,

    /// Only entries slower than N milliseconds (duration_ms > N)
    #[arg(long = "slow-above", value_name = "MS")]
    pub slow_above: Option<f64>,

    /// Result order (default: newest first)
    #[arg(long, value_enum, default_value = "time")]
    pub sort: SortOrder,

    /// Time range start (ISO 8601)
    #[arg(short, long)]
    pub start: Option<String>,

    /// Time range end (ISO 8601)
    #[arg(short = 'e', long)]
    pub end: Option<String>,

    /// Relative time filter (e.g., "1h", "1d", "1w")
    #[arg(short, long)]
    pub last: Option<String>,

    /// Maximum number of results
    #[arg(short = 'n', long, default_value = "100")]
    pub limit: usize,

    /// Offset for pagination
    #[arg(short, long, default_value = "0")]
    pub offset: usize,

    /// Compact output - omit null fields
    #[arg(long)]
    pub compact: bool,

    /// Field selection - only return specified fields (comma-separated)
    #[arg(long, value_delimiter = ',')]
    pub fields: Option<Vec<String>>,

    /// Include N logs before and after each matching result
    #[arg(long, value_name = "N")]
    pub context: Option<usize>,
}

impl SearchCommand {
    pub async fn execute(&self) -> Result<u8> {
        let status = self.status.as_deref().map(parse_status_range).transpose()?;
        if self.sort == SortOrder::Relevance && self.query.is_none() {
            return Err(CliError::new(
                ErrorCode::Usage,
                "--sort relevance requires a full-text --query",
            )
            .with_hint("Add --query <text>, or drop --sort to get newest-first ordering.")
            .into());
        }

        let mut envelope = Envelope::new("search");
        let config = Config::load()?;
        let (searcher, sync_report) = crate::cmd::prepare_read(config)?;
        envelope.set_sync(sync_report);

        let options = SearchOptions {
            query: self.query.clone(),
            level: self.level.clone(),
            log_id: None,
            request_id: self.request_id.clone(),
            user_id: self.user_id.clone(),
            route: self.route.clone(),
            source: self.source.clone().map(|s| s.to_lowercase()),
            method: self.method.clone(),
            parse_format: self.parse_format.clone().map(|s| s.to_lowercase()),
            status,
            slow_above: self.slow_above,
            start: self.start.clone(),
            end: self.end.clone(),
            last: self.last.clone(),
            limit: self.limit.min(10000),
            offset: self.offset,
            sort: self.sort,
        };

        if let Some(context) = self.context {
            let output = searcher.search_with_context(options, context)?;

            let windows_json: Vec<serde_json::Value> = output
                .windows
                .iter()
                .map(|window| {
                    Ok(serde_json::json!({
                        "target": crate::schema::format_log_entry(
                            &window.target, self.compact, self.fields.as_deref())?,
                        "before": crate::schema::format_log_entries(
                            &window.before, self.compact, self.fields.as_deref())?,
                        "after": crate::schema::format_log_entries(
                            &window.after, self.compact, self.fields.as_deref())?,
                    }))
                })
                .collect::<serde_json::Result<Vec<_>>>()?;

            let count = windows_json.len();
            let empty = count == 0;
            if crate::is_jsonl() {
                envelope.emit_jsonl(&windows_json)?;
            } else {
                let meta = list_meta(
                    count,
                    output.total,
                    Some(self.limit),
                    Some(self.offset),
                    serde_json::Value::Object(output.filters),
                    output.time_range,
                );
                envelope.emit(serde_json::json!({ "results": windows_json }), Some(meta))?;
            }
            return Ok(if empty { 2 } else { 0 });
        }

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
                Some(self.limit),
                Some(self.offset),
                serde_json::Value::Object(output.filters),
                output.time_range,
            );
            envelope.emit(serde_json::json!({ "results": results_json }), Some(meta))?;
        }

        Ok(if count == 0 { 2 } else { 0 })
    }
}

/// Parse "500" or "500-599" into an inclusive status-code range.
fn parse_status_range(input: &str) -> Result<(u16, u16)> {
    let parse_code = |s: &str| -> Result<u16> {
        s.trim().parse::<u16>().map_err(|_| {
            CliError::new(
                ErrorCode::Usage,
                format!("Invalid status code: {:?}", s.trim()),
            )
            .with_hint("Use a number (--status 500) or an inclusive range (--status 500-599).")
            .into()
        })
    };
    match input.split_once('-') {
        Some((low, high)) => {
            let (low, high) = (parse_code(low)?, parse_code(high)?);
            if low > high {
                return Err(CliError::new(
                    ErrorCode::Usage,
                    format!("Status range is inverted: {}-{}", low, high),
                )
                .with_hint("The lower bound must come first, e.g. --status 500-599.")
                .into());
            }
            Ok((low, high))
        }
        None => {
            let code = parse_code(input)?;
            Ok((code, code))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_status_exact_and_range() {
        assert_eq!(parse_status_range("500").unwrap(), (500, 500));
        assert_eq!(parse_status_range("500-599").unwrap(), (500, 599));
        assert!(parse_status_range("abc").is_err());
        assert!(parse_status_range("599-500").is_err());
    }
}
