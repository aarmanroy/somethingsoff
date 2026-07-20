//! Message masking and fingerprinting shared by error grouping and
//! template mining.
//!
//! `normalize_template` + `generate_fingerprint` define the `errors`
//! command's stable group identity: the same log line must yield the same
//! fingerprint in every invocation, forever. Do not change their masking
//! rules or ordering — extend `mask_extended` instead, which is free to
//! evolve because `patterns` template IDs are only stable within a response.

use once_cell::sync::Lazy;
use regex::Regex;
use sha2::Digest;

/// Generate a deterministic fingerprint from error name and message
pub fn generate_fingerprint(name: &str, message: &str) -> String {
    let mut hasher = sha2::Sha256::new();
    sha2::Digest::update(&mut hasher, name.as_bytes());
    sha2::Digest::update(&mut hasher, b"|");
    sha2::Digest::update(&mut hasher, message.as_bytes());
    let result = sha2::Digest::finalize(hasher);
    hex::encode(&result[..8]) // First 8 bytes = 16 hex chars
}

#[allow(clippy::unwrap_used)] // compile-time-constant patterns
static UUID: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"\b[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}\b")
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

/// Collapse the variable parts of an error message into placeholders so
/// near-identical errors (same failure, different UUID/host/duration)
/// group together ("drain-lite"). The returned template is both the
/// grouping key input and a human/agent-readable summary.
///
/// Masking order matters: specific patterns run before general ones.
pub fn normalize_template(message: &str) -> String {
    let masked = UUID.replace_all(message, "<uuid>");
    let masked = IP.replace_all(&masked, "<ip>");
    let masked = HEX.replace_all(&masked, "<hex>");
    let masked = QUOTED.replace_all(&masked, "<str>");
    let masked = NUM.replace_all(&masked, "<num>");
    masked.into_owned()
}

// ISO8601 timestamp, T- or space-separated: 2026-07-20T03:58:04.581Z
#[allow(clippy::unwrap_used)]
static ISO8601: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"\b\d{4}-\d{2}-\d{2}[Tt ]\d{2}:\d{2}:\d{2}(\.\d+)?(Z|[+-]\d{2}:?\d{2})?\b").unwrap()
});
// Absolute path with >=2 segments so ratios like "20/20" stay literal.
#[allow(clippy::unwrap_used)]
static PATH: Lazy<Regex> = Lazy::new(|| Regex::new(r"/[\w.\-]+(/[\w.\-]+)+/?").unwrap());
// Hostname with >=3 labels; short names like "db-3.internal" stay
// literal, matching normalize_template's existing template style.
#[allow(clippy::unwrap_used)]
static HOST: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"\b[A-Za-z0-9][A-Za-z0-9\-]*(\.[A-Za-z0-9\-]+){2,}\b").unwrap());
// Number glued to a time unit ("1500ms", "2.5s") — before NUM.
#[allow(clippy::unwrap_used)]
static DURATION: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"\b\d+(\.\d+)?(ns|us|µs|ms|s|m|h)\b").unwrap());

/// Drain pre-mask: `normalize_template`'s rules plus timestamp, path,
/// hostname, and duration masks. Used only by template mining
/// (`patterns`) — never by the `errors` fingerprint path, whose masking
/// is frozen for cross-run identity. No placeholder contains a digit,
/// quote, slash, or dot, so this function is idempotent.
pub fn mask_extended(message: &str) -> String {
    let masked = UUID.replace_all(message, "<uuid>");
    let masked = ISO8601.replace_all(&masked, "<ts>");
    let masked = IP.replace_all(&masked, "<ip>");
    let masked = PATH.replace_all(&masked, "<path>");
    let masked = HOST.replace_all(&masked, "<host>");
    let masked = HEX.replace_all(&masked, "<hex>");
    let masked = QUOTED.replace_all(&masked, "<str>");
    let masked = DURATION.replace_all(&masked, "<dur>");
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

    #[test]
    fn test_mask_extended_timestamps_paths_hosts_durations() {
        assert_eq!(
            mask_extended("2026-07-20T03:58:04.581Z GET /api/users/42 200 in 1500ms"),
            "<ts> GET <path> <num> in <dur>"
        );
        assert_eq!(
            mask_extended("Connection timeout to db-3.eu-1.internal after 2.5s"),
            "Connection timeout to <host> after <dur>"
        );
        // Short hostnames don't become <host>, and ratios don't become
        // <path> — digits inside them still mask as <num>, exactly like
        // normalize_template.
        assert_eq!(
            mask_extended("db-3.internal replied 20/20 ok"),
            "db-<num>.internal replied <num>/<num> ok"
        );
    }

    #[test]
    fn test_mask_extended_is_superset_of_normalize_template() {
        let msg = "Upstream 10.0.3.7:8080 returned 502 for 550e8400-e29b-41d4-a716-446655440000";
        assert_eq!(mask_extended(msg), normalize_template(msg));
    }

    #[test]
    fn test_mask_extended_idempotent_on_samples() {
        for msg in [
            "2026-07-20 03:58:04 rclone: Copied /Users/x/file.txt to remote:backup in 320ms",
            "error at src/main.rs line 42",
            "user 'alice' fetched https://api.example.com/v1/items?page=2",
        ] {
            let once = mask_extended(msg);
            assert_eq!(mask_extended(&once), once, "not idempotent for: {msg}");
        }
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

            #[test]
            fn proptest_mask_extended_idempotent(message in ".*") {
                let once = mask_extended(&message);
                let twice = mask_extended(&once);
                assert_eq!(once, twice);
            }
        }
    }
}
