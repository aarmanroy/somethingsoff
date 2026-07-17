//! Learn command - suggest regex patterns from sample log lines.

use anyhow::Result;
use clap::Args;

use crate::output::{CliError, Envelope, ErrorCode};
use crate::parser::learn::suggest_patterns;

/// Suggest regex patterns for unknown log formats from a sample line.
#[derive(Args)]
pub struct LearnCommand {
    /// Sample log line to analyze
    #[arg(long)]
    pub sample: String,
}

impl LearnCommand {
    pub async fn execute(&self) -> Result<u8> {
        let envelope = Envelope::new("learn");
        let sample = self.sample.trim();
        if sample.is_empty() {
            return Err(
                CliError::new(ErrorCode::ParseError, "Sample log line cannot be empty")
                    .with_hint(
                        "Pass a real log line: somethingsoff learn --sample '<paste a line here>'",
                    )
                    .into(),
            );
        }

        let suggestions = suggest_patterns(sample);
        let empty = suggestions.is_empty();

        if crate::is_jsonl() {
            envelope.emit_jsonl(&suggestions)?;
        } else {
            envelope.emit(serde_json::json!({ "suggestions": suggestions }), None)?;
        }

        Ok(if empty { 2 } else { 0 })
    }
}
