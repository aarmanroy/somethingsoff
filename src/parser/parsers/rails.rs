//! Rails / ActiveSupport::TaggedLogging parser.
//!
//! The default Rails development and production log, which is what a Rails
//! app actually writes to `log/development.log`:
//!
//! ```text
//! [f84f574f-...] Started GET "/login" for 192.168.65.1 at 2026-03-24 17:58:06 +0000
//! [f84f574f-...] Processing by SessionsController#new as HTML
//! [f84f574f-...] Completed 302 Found in 11ms (ActiveRecord: 0.4ms | GC: 0.0ms)
//! ```
//!
//! Every field worth filtering on is present in that text and none of it was
//! extracted: a 285k-line Rails log indexed as 100% `raw`, so `--request-id`,
//! `--status` and `--slow-above` all returned nothing, and the request UUID
//! became part of every mined error template.
//!
//! Only `Started` lines carry a timestamp. The rest of a request inherits
//! ingest time, which is why the request id matters: it is what ties the
//! lines of one request together when their timestamps cannot. Detection is
//! per line, so a UUID tag is enough to claim a line — every line of the
//! request needs its id, not just the three Rails formats below.

use crate::parser::{log_format::LogFormat, LogParser};
use crate::schema::RawLogEntry;
use once_cell::sync::Lazy;
use regex::Regex;

pub struct RailsParser;

/// The `[<request id>]` tag ActiveSupport::TaggedLogging prepends. Rails uses
/// a UUID by default, but the tag is user-configurable, so this accepts any
/// bracketed token that is not obviously prose.
static TAG_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"^\[(?P<tag>[A-Za-z0-9][A-Za-z0-9._:-]{7,})\]\s+(?P<rest>.*)$").unwrap()
});

/// `Started GET "/login" for 1.2.3.4 at 2026-03-24 17:58:06 +0000`
static STARTED_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r#"^Started\s+(?P<method>[A-Z]+)\s+"(?P<route>[^"]*)""#).unwrap());

/// The trailing `at <timestamp>` of a `Started` line, matched separately
/// rather than as one anchored expression: Rails interpolates variants
/// between the route and the clause (`Started GET "/cable" [WebSocket] for
/// 1.2.3.4 at ...`), and requiring adjacency silently cost those lines their
/// only real timestamp.
static STARTED_AT_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(
        r"\bat\s+(?P<timestamp>\d{4}-\d{2}-\d{2}[ T]\d{2}:\d{2}:\d{2}(?:\.\d+)?(?:\s*[+-]\d{4})?)\s*$",
    )
    .unwrap()
});

/// `Completed 302 Found in 11ms (ActiveRecord: 0.4ms | GC: 0.0ms)`
static COMPLETED_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"^Completed\s+(?P<status>\d{3})\b[^)]*?\bin\s+(?P<duration>\d+(?:\.\d+)?)ms")
        .unwrap()
});

/// `Processing by SessionsController#new as HTML`
static PROCESSING_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"^Processing by\s+(?P<controller>\S+)").unwrap());

impl LogParser for RailsParser {
    fn parse(&self, line: &str, _source: &str) -> Option<RawLogEntry> {
        let caps = TAG_RE.captures(line)?;
        let request_id = caps.name("tag")?.as_str().to_string();
        let rest = caps.name("rest")?.as_str().trim();
        if rest.is_empty() {
            return None;
        }

        let mut entry = RawLogEntry {
            request_id: Some(request_id),
            message: Some(rest.to_string()),
            // Rails writes no severity on the line, so fall back to the same
            // text sniffing the raw path uses. Without this, extracting
            // structure would *cost* accuracy: every exception would index as
            // `info` simply because a parser claimed the line.
            level: crate::parser::parsers::sniff_level(rest),
            ..Default::default()
        };

        if let Some(started) = STARTED_RE.captures(rest) {
            entry.method = started.name("method").map(|m| m.as_str().to_string());
            entry.route = started.name("route").map(|m| m.as_str().to_string());
            // The one line per request that carries a real clock reading.
            entry.timestamp = STARTED_AT_RE
                .captures(rest)
                .and_then(|at| at.name("timestamp"))
                .map(|m| normalize_rails_time(m.as_str()));
        } else if let Some(completed) = COMPLETED_RE.captures(rest) {
            entry.status_code = completed
                .name("status")
                .and_then(|m| m.as_str().parse::<u16>().ok());
            entry.duration_ms = completed
                .name("duration")
                .and_then(|m| m.as_str().parse::<f64>().ok());
            // The status is better evidence than the words on the line: a
            // 5xx is an error even though Rails logs it at info, and
            // "Completed 200 OK" should not become an error because the
            // response body mentioned one.
            if let Some(status) = entry.status_code {
                entry.level = Some(
                    match status {
                        500..=599 => "error",
                        400..=499 => "warn",
                        _ => "info",
                    }
                    .to_string(),
                );
            }
        } else if let Some(processing) = PROCESSING_RE.captures(rest) {
            entry.route = processing
                .name("controller")
                .map(|m| m.as_str().to_string());
        }

        Some(entry)
    }

    fn format_name(&self) -> &str {
        "rails"
    }

    fn format(&self) -> LogFormat {
        LogFormat::Rails
    }
}

/// `2026-03-24 17:58:06 +0000` -> `2026-03-24T17:58:06+00:00`.
///
/// Rails writes the offset in the compact `+0000` form, which RFC3339 does
/// not accept — it wants `+00:00`. Emitting the compact form failed
/// normalization, so the entry silently fell back to ingest time and the one
/// real clock reading in the whole request was thrown away. Left alone when
/// there is no offset; the shared normalizer handles a naive timestamp.
fn normalize_rails_time(raw: &str) -> String {
    let trimmed = raw.trim();
    match trimmed.rsplit_once(' ') {
        Some((datetime, offset))
            if offset.len() == 5
                && (offset.starts_with('+') || offset.starts_with('-'))
                && offset[1..].chars().all(|c| c.is_ascii_digit()) =>
        {
            let (hours, minutes) = offset.split_at(3);
            format!("{}{}:{}", datetime.replace(' ', "T"), hours, minutes)
        }
        _ => trimmed.replace(' ', "T"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(line: &str) -> RawLogEntry {
        RailsParser.parse(line, "test").expect("should parse")
    }

    #[test]
    fn test_started_line_yields_route_method_and_real_timestamp() {
        let entry = parse(
            r#"[f84f574f-d5c5-43d7-9abb-8340d3894851] Started GET "/login" for 192.168.65.1 at 2026-03-24 17:58:06 +0000"#,
        );
        assert_eq!(
            entry.request_id.as_deref(),
            Some("f84f574f-d5c5-43d7-9abb-8340d3894851")
        );
        assert_eq!(entry.method.as_deref(), Some("GET"));
        assert_eq!(entry.route.as_deref(), Some("/login"));
        // RFC3339 form, so `normalize_timestamp` accepts it and the entry
        // keeps its real clock reading instead of borrowing ingest time.
        assert_eq!(
            entry.timestamp.as_deref(),
            Some("2026-03-24T17:58:06+00:00")
        );
        assert!(
            crate::schema::normalize_timestamp(entry.timestamp.as_deref().unwrap()).is_some(),
            "the emitted timestamp must survive normalization"
        );
    }

    /// Rails interpolates variants between the route and the `at` clause.
    /// Requiring them to be adjacent cost these lines their only timestamp.
    #[test]
    fn test_websocket_started_line_still_yields_its_timestamp() {
        let entry = parse(
            r#"[3a0ff09d-2896-452c-b572-13fa0740c319] Started GET "/897362095/cable" [WebSocket] for 192.168.65.1 at 2026-03-31 09:22:38 +0000"#,
        );
        assert_eq!(entry.route.as_deref(), Some("/897362095/cable"));
        assert_eq!(
            entry.timestamp.as_deref(),
            Some("2026-03-31T09:22:38+00:00")
        );
    }

    /// Non-UTC offsets keep their sign and magnitude.
    #[test]
    fn test_offset_is_converted_to_rfc3339_form() {
        assert_eq!(
            normalize_rails_time("2026-03-24 17:58:06 +0530"),
            "2026-03-24T17:58:06+05:30"
        );
        assert_eq!(
            normalize_rails_time("2026-03-24 17:58:06 -0800"),
            "2026-03-24T17:58:06-08:00"
        );
    }

    #[test]
    fn test_completed_line_yields_status_and_duration() {
        let entry = parse(
            "[4bdf672a-b42e-4f02-b53a-8d73798f0b79] Completed 302 Found in 11ms (ActiveRecord: 0.0ms | GC: 0.0ms)",
        );
        assert_eq!(entry.status_code, Some(302));
        assert_eq!(entry.duration_ms, Some(11.0));
        assert_eq!(entry.level.as_deref(), Some("info"));
    }

    /// Rails logs a 500 at info severity; the status is the real signal.
    #[test]
    fn test_server_error_status_is_promoted_to_error_level() {
        let entry = parse(
            "[abcdef01-2345-6789-abcd-ef0123456789] Completed 500 Internal Server Error in 87ms",
        );
        assert_eq!(entry.status_code, Some(500));
        assert_eq!(entry.level.as_deref(), Some("error"));
        let client = parse("[abcdef01-2345-6789-abcd-ef0123456789] Completed 404 Not Found in 3ms");
        assert_eq!(client.level.as_deref(), Some("warn"));
    }

    #[test]
    fn test_tag_is_stripped_from_the_message() {
        let entry = parse(
            "[4bdf672a-b42e-4f02-b53a-8d73798f0b79] Processing by EventsController#index as */*",
        );
        assert_eq!(
            entry.message.as_deref(),
            Some("Processing by EventsController#index as */*")
        );
        assert_eq!(entry.route.as_deref(), Some("EventsController#index"));
        // The request id lives in its own field, not in the mined template.
        assert!(!entry.message.as_deref().unwrap().contains('['));
    }

    /// A tagged line Rails did not generate still gets its request id, which
    /// is the field that makes `get --request-id` useful across a request.
    #[test]
    fn test_arbitrary_tagged_line_keeps_its_request_id() {
        let entry = parse(
            "[c26159af-b7b1-4dcb-9e73-96cb7da88d8e] Cannot render console from 192.168.65.1!",
        );
        assert_eq!(
            entry.request_id.as_deref(),
            Some("c26159af-b7b1-4dcb-9e73-96cb7da88d8e")
        );
        assert!(entry
            .message
            .as_deref()
            .unwrap()
            .starts_with("Cannot render"));
    }

    /// Claiming a line must never cost accuracy against the raw fallback:
    /// Rails puts no severity on the line, so the text still has to be read.
    #[test]
    fn test_level_is_sniffed_when_rails_gives_none() {
        let entry = parse(
            "[c26159af-b7b1-4dcb-9e73-96cb7da88d8e] ERROR ActionView::Template::Error (undefined method)",
        );
        assert_eq!(entry.level.as_deref(), Some("error"));
    }

    /// ...but the status code outranks the words, so a successful response
    /// whose body mentions an error is not itself an error.
    #[test]
    fn test_status_outranks_sniffed_text_on_completed_lines() {
        let entry = parse(
            "[c26159af-b7b1-4dcb-9e73-96cb7da88d8e] Completed 200 OK in 5ms (Views: 1.0ms | error_count: 0)",
        );
        assert_eq!(entry.status_code, Some(200));
        assert_eq!(entry.level.as_deref(), Some("info"));
    }

    #[test]
    fn test_non_rails_lines_are_declined() {
        for line in [
            "Started GET \"/login\"",           // no tag
            "[x] short tag",                    // tag too short to be an id
            "plain log line",                   //
            "[2026-03-24 17:58:06] syslog-ish", // a bracketed timestamp, not a tag
        ] {
            assert!(
                RailsParser.parse(line, "test").is_none(),
                "should decline: {line}"
            );
        }
    }
}
