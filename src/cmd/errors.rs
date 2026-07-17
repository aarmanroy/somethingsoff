//! Error aggregation command - group and count errors by fingerprint
//!
//! Uses a custom Tantivy collector (`ErrorAggCollector`) to group errors
//! during the search phase — processing **all** matching error documents
//! without materialising `LogEntry` objects or imposing a 10K limit.

use anyhow::Result;
use clap::Args;
use serde::Serialize;
use sha2::Digest;

use crate::config::Config;
use crate::index::searcher::SearchOptions;
use crate::output::{list_meta, Envelope};

/// Aggregate and analyze error logs
#[derive(Args)]
pub struct ErrorsCommand {
    /// Time window (5m, 1h, 1d, 7d)
    #[arg(short, long, default_value = "24h")]
    pub last: String,

    /// Maximum number of error groups to return
    #[arg(short = 'n', long, default_value = "10")]
    pub limit: usize,
}

/// Error group with aggregated statistics
#[derive(Debug, Serialize)]
pub struct ErrorGroup {
    /// Fingerprint hash (error name + masked template)
    pub fingerprint: String,
    /// Error name/type
    pub error_name: Option<String>,
    /// Sample error message (first seen in the group)
    pub error_message: Option<String>,
    /// Masked message template shared by the whole group
    pub template: String,
    /// Total occurrence count
    pub count: usize,
    /// Number of unique affected users
    pub affected_users: usize,
    /// First occurrence timestamp
    pub first_seen: String,
    /// Most recent occurrence timestamp
    pub last_seen: String,
    /// Sample log IDs for reference
    pub sample_log_ids: Vec<String>,
}

impl ErrorsCommand {
    pub async fn execute(&self) -> Result<u8> {
        let mut envelope = Envelope::new("errors");
        let config = Config::load()?;
        let (searcher, sync_report) = crate::cmd::prepare_read(config)?;
        envelope.set_sync(sync_report);

        let options = SearchOptions {
            level: Some("error".to_string()),
            last: Some(self.last.clone()),
            ..Default::default()
        };

        // Use streaming error aggregation (no 10K limit)
        let result = searcher.errors_query(&options, self.limit)?;

        // Convert aggregated groups to output format
        let error_groups: Vec<ErrorGroup> = result
            .groups
            .into_iter()
            .map(|g| ErrorGroup {
                fingerprint: g.fingerprint,
                error_name: g.error_name,
                error_message: g.error_message,
                template: g.template,
                count: g.count,
                affected_users: g.affected_users.len(),
                first_seen: g.first_seen,
                last_seen: g.last_seen,
                sample_log_ids: g.sample_log_ids,
            })
            .collect();

        let total_errors = result.total_errors;
        let total_groups = error_groups.len();

        if crate::is_jsonl() {
            envelope.emit_jsonl(&error_groups)?;
        } else {
            let meta = list_meta(
                total_groups,
                total_errors,
                Some(self.limit),
                None,
                serde_json::json!({"level": "error", "last": self.last}),
                None,
            );
            envelope.emit(
                serde_json::json!({
                    "total_errors": total_errors,
                    "total_groups": total_groups,
                    "groups": error_groups,
                }),
                Some(meta),
            )?;
        }

        Ok(if total_errors == 0 { 2 } else { 0 })
    }
}

/// Generate a deterministic fingerprint from error name and message
pub fn generate_fingerprint(name: &str, message: &str) -> String {
    let mut hasher = sha2::Sha256::new();
    sha2::Digest::update(&mut hasher, name.as_bytes());
    sha2::Digest::update(&mut hasher, b"|");
    sha2::Digest::update(&mut hasher, message.as_bytes());
    let result = sha2::Digest::finalize(hasher);
    hex::encode(&result[..8]) // First 8 bytes = 16 hex chars
}

/// Collapse the variable parts of an error message into placeholders so
/// near-identical errors (same failure, different UUID/host/duration)
/// group together ("drain-lite"). The returned template is both the
/// grouping key input and a human/agent-readable summary.
///
/// Masking order matters: specific patterns run before general ones.
pub fn normalize_template(message: &str) -> String {
    use once_cell::sync::Lazy;
    use regex::Regex;

    #[allow(clippy::unwrap_used)] // compile-time-constant patterns
    static UUID: Lazy<Regex> = Lazy::new(|| {
        Regex::new(
            r"\b[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}\b",
        )
        .unwrap()
    });
    #[allow(clippy::unwrap_used)]
    static IP: Lazy<Regex> =
        Lazy::new(|| Regex::new(r"\b\d{1,3}\.\d{1,3}\.\d{1,3}\.\d{1,3}(:\d+)?\b").unwrap());
    #[allow(clippy::unwrap_used)]
    static HEX: Lazy<Regex> = Lazy::new(|| Regex::new(r"\b[0-9a-fA-F]{8,}\b").unwrap());
    #[allow(clippy::unwrap_used)]
    static QUOTED: Lazy<Regex> = Lazy::new(|| Regex::new(r#""[^"]*"|'[^']*'"#).unwrap());
    // Leading \b only: digits glued to a trailing unit ("1500ms") must
    // still mask, while digits inside identifiers ("sha256") must not.
    #[allow(clippy::unwrap_used)]
    static NUM: Lazy<Regex> = Lazy::new(|| Regex::new(r"\b\d+(\.\d+)?").unwrap());

    let masked = UUID.replace_all(message, "<uuid>");
    let masked = IP.replace_all(&masked, "<ip>");
    let masked = HEX.replace_all(&masked, "<hex>");
    let masked = QUOTED.replace_all(&masked, "<str>");
    let masked = NUM.replace_all(&masked, "<num>");
    masked.into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_generate_fingerprint_deterministic() {
        let fp1 = generate_fingerprint("ConnectionError", "Failed to connect");
        let fp2 = generate_fingerprint("ConnectionError", "Failed to connect");
        assert_eq!(fp1, fp2, "Same input should produce same fingerprint");
        assert_eq!(fp1.len(), 16, "Fingerprint should be 16 characters");
    }

    #[test]
    fn test_normalize_template_masks_variable_parts() {
        assert_eq!(
            normalize_template("Connection timeout to db-3.internal after 1500ms"),
            "Connection timeout to db-<num>.internal after <num>ms"
        );
        assert_eq!(
            normalize_template(
                "Upstream 10.0.3.7:8080 returned 502 for 550e8400-e29b-41d4-a716-446655440000"
            ),
            "Upstream <ip> returned <num> for <uuid>"
        );
        assert_eq!(
            normalize_template("Failed to acquire lock on 'orders' (txn deadbeefcafe1234)"),
            "Failed to acquire lock on <str> (txn <hex>)"
        );
    }

    #[test]
    fn test_normalize_template_groups_near_duplicates() {
        let a = normalize_template(
            "Payment gw-1.internal rejected txn 111e8400-e29b-41d4-a716-446655440000 after 300ms",
        );
        let b = normalize_template(
            "Payment gw-9.internal rejected txn 222e8400-e29b-41d4-a716-446655440999 after 4500ms",
        );
        assert_eq!(a, b, "near-duplicate messages must share a template");

        let fp_a = generate_fingerprint("PaymentError", &a);
        let fp_b = generate_fingerprint("PaymentError", &b);
        assert_eq!(fp_a, fp_b);
    }

    #[test]
    fn test_normalize_template_stable_for_static_messages() {
        let msg = "database connection pool exhausted";
        assert_eq!(normalize_template(msg), msg);
    }

    #[test]
    fn test_generate_fingerprint_different() {
        let fp1 = generate_fingerprint("ConnectionError", "Failed to connect");
        let fp2 = generate_fingerprint("ConnectionError", "Timeout");
        let fp3 = generate_fingerprint("TimeoutError", "Failed to connect");
        assert_ne!(
            fp1, fp2,
            "Different messages should produce different fingerprints"
        );
        assert_ne!(
            fp1, fp3,
            "Different names should produce different fingerprints"
        );
    }

    #[cfg(test)]
    mod proptests {
        use super::*;
        use proptest::prelude::*;

        proptest! {
            #[test]
            fn proptest_fingerprint_determinism(
                name in ".*",
                message in ".*"
            ) {
                let fp1 = generate_fingerprint(&name, &message);
                let fp2 = generate_fingerprint(&name, &message);
                assert_eq!(fp1, fp2);
                assert_eq!(fp1.len(), 16);
            }
        }
    }
}
