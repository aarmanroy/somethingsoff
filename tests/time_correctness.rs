//! Time correctness: ordering, timestamp normalization, and the guarantee
//! that every indexed entry is reachable by a time filter.
//!
//! These cover three bugs the rest of the suite was structurally unable to
//! see, because every other test builds a *single-segment* index in *one*
//! sync pass and never feeds a log4j line to a time filter:
//!
//! 1. `search` ordered by the `timestamp` STRING fast field, which sorts by
//!    segment-local term ordinal — correct-looking on one segment, wrong on
//!    any index synced more than once.
//! 2. log4j timestamps got an extra `.000` appended, so no Java line ever
//!    matched `--last`/`--start`/`--end`.
//! 3. A timestamp that failed normalization was stored verbatim, leaving the
//!    entry with no `timestamp_ms` and unreachable by any time query.

use assert_cmd::Command;
use std::fs;
use std::io::Write;
use std::path::Path;
use tempfile::TempDir;

fn somethingsoff(project_dir: &Path) -> Command {
    let mut cmd = Command::cargo_bin("somethingsoff").unwrap();
    cmd.current_dir(project_dir)
        .env("SOMETHINGSOFF_BASE_DIR", project_dir.join(".somethingsoff"));
    cmd
}

fn setup_project() -> (TempDir, std::path::PathBuf) {
    let temp = TempDir::new().unwrap();
    let logs_dir = temp.path().join("logs");
    fs::create_dir(&logs_dir).unwrap();
    let log_file = logs_dir.join("app.log");
    (temp, log_file)
}

fn append(path: &Path, body: &str) {
    let mut f = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .unwrap();
    f.write_all(body.as_bytes()).unwrap();
}

/// Run a read command and return its parsed envelope.
/// Asserts on the envelope rather than the exit status: exit 2 means "zero
/// results", which the contract defines as success, so a query that
/// legitimately matches nothing must not read as a failure here.
fn query(project: &Path, args: &[&str]) -> serde_json::Value {
    let out = somethingsoff(project).args(args).assert();
    let stdout = String::from_utf8(out.get_output().stdout.clone()).unwrap();
    let value: serde_json::Value =
        serde_json::from_str(&stdout).unwrap_or_else(|e| panic!("not JSON: {e}\n{stdout}"));
    assert_eq!(value["ok"], true, "command failed: {stdout}");
    value
}

fn segment_count(project: &Path) -> usize {
    let meta = fs::read_to_string(project.join(".somethingsoff/index/meta.json")).unwrap();
    let meta: serde_json::Value = serde_json::from_str(&meta).unwrap();
    meta["segments"].as_array().map(Vec::len).unwrap_or(0)
}

/// Regression: newest-first ordering across a multi-segment index.
///
/// Each batch is one sync pass, so each becomes its own segment. The batch
/// written *first* holds the globally newest timestamps — under the old
/// string-field ordering the collector returned a later segment's documents
/// instead, i.e. `search` confidently answered with the wrong entries.
#[test]
fn test_newest_first_across_multiple_segments() {
    let (temp, log_file) = setup_project();

    // (batch name, timestamp) — deliberately NOT in chronological order.
    let batches = [
        ("seg1", "2026-05-01"),
        ("seg2", "2026-02-01"),
        ("seg3", "2026-03-01"),
    ];
    for (name, day) in batches {
        let mut body = String::new();
        for i in 0..3000 {
            body.push_str(&format!(
                r#"{{"timestamp":"{day}T10:00:00.000Z","level":"error","message":"{name}-item-{i}"}}"#
            ));
            body.push('\n');
        }
        append(&log_file, &body);
        // One read = one sync = one commit = one segment.
        somethingsoff(temp.path())
            .args(["search", "-n", "1"])
            .assert()
            .success();
    }

    // Precondition. If tantivy's merge policy collapses these, this test is no
    // longer exercising the bug it was written for and must say so loudly
    // rather than pass for the wrong reason.
    let segments = segment_count(temp.path());
    assert!(
        segments > 1,
        "fixture no longer produces a multi-segment index ({segments} segment) — \
         this test cannot detect segment-local ordering bugs in this state"
    );

    let result = query(temp.path(), &["--quiet", "search", "-n", "3", "--compact"]);
    let messages: Vec<&str> = result["data"]["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["message"].as_str().unwrap())
        .collect();

    for message in &messages {
        assert!(
            message.starts_with("seg1-"),
            "newest-first must return the 2026-05 batch across {segments} segments, got {messages:?}"
        );
    }
}

/// Regression: log4j lines carry millis by default; the parser appended a
/// second `.000`, producing `10:00:01.500.000Z` — unnormalizable, so the
/// entry got no `timestamp_ms` and matched no time filter.
#[test]
fn test_log4j_millis_are_reachable_by_time_filter() {
    let (temp, log_file) = setup_project();
    append(
        &log_file,
        "2026-08-09 10:00:01.500 ERROR [main] com.example.Foo - database connection failed\n\
         2026-08-09 10:00:02,750 ERROR [main] com.example.Bar - retry exhausted\n\
         2026-08-09 10:00:03 ERROR [main] com.example.Baz - no millis here\n",
    );

    let all = query(temp.path(), &["--quiet", "search", "--level", "error"]);
    assert_eq!(all["meta"]["total"], 3, "all three lines should index");

    // The actual bug: present in the index, invisible to every time query.
    let ranged = query(
        temp.path(),
        &[
            "--quiet",
            "search",
            "--level",
            "error",
            "--start",
            "2026-08-09T00:00:00Z",
        ],
    );
    assert_eq!(
        ranged["meta"]["total"], 3,
        "log4j entries must be reachable by a time range, not just an unfiltered scan"
    );

    // Both millis separators normalize to the canonical shape, and a line
    // without millis still gets .000 rather than being dropped.
    let stamps: Vec<&str> = all["data"]["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["timestamp"].as_str().unwrap())
        .collect();
    assert!(
        stamps.contains(&"2026-08-09T10:00:01.500Z"),
        "got {stamps:?}"
    );
    assert!(
        stamps.contains(&"2026-08-09T10:00:02.750Z"),
        "got {stamps:?}"
    );
    assert!(
        stamps.contains(&"2026-08-09T10:00:03.000Z"),
        "got {stamps:?}"
    );
}

/// Regression: a timestamp that can't be normalized (Next.js writes a
/// time-only `01:57:10.385`) used to be stored verbatim. The entry was
/// detected as valid JSON, reported as such by `stats --by-format`, and was
/// unreachable by every time query — a confident wrong answer.
#[test]
fn test_unnormalizable_timestamp_stays_queryable() {
    let (temp, log_file) = setup_project();
    append(
        &log_file,
        "{\"timestamp\":\"01:57:10.385\",\"level\":\"error\",\"message\":\"time only\"}\n",
    );

    let ranged = query(
        temp.path(),
        &["--quiet", "search", "--level", "error", "--last", "24h"],
    );
    assert_eq!(
        ranged["meta"]["total"], 1,
        "an entry with an unparseable timestamp must still be reachable by time"
    );

    let entry = &ranged["data"]["results"][0];
    // Falls back to ingest time in the canonical shape...
    let stamp = entry["timestamp"].as_str().unwrap();
    assert!(
        stamp.ends_with('Z') && stamp.len() == 24,
        "timestamp must be canonical, got {stamp}"
    );
    // ...and the original is preserved rather than silently dropped.
    assert_eq!(
        entry["attributes"]["original_timestamp"], "01:57:10.385",
        "the raw timestamp must survive in attributes"
    );
}

/// Re-ingesting the same file must not duplicate entries whose timestamp was
/// replaced by ingest time. The substituted clock value varies per pass, so
/// it must stay out of `log_id` — identity falls back to position + content.
#[test]
fn test_unnormalizable_timestamps_still_dedup_on_reingest() {
    let (temp, log_file) = setup_project();
    append(
        &log_file,
        "{\"timestamp\":\"01:57:10.385\",\"level\":\"error\",\"message\":\"repeatable\"}\n",
    );

    somethingsoff(temp.path())
        .args(["search", "-n", "1"])
        .assert()
        .success();
    somethingsoff(temp.path())
        .args(["index", "rebuild"])
        .assert()
        .success();

    let after = query(temp.path(), &["--quiet", "search", "--level", "error"]);
    assert_eq!(
        after["meta"]["total"], 1,
        "a rebuild must not duplicate an entry that borrowed ingest time"
    );
}

/// A line carrying its own timestamp must be dated by that timestamp, even
/// when no structured parser claims the line.
///
/// `<ISO-8601> <LEVEL> <message>` is what most hand-rolled and Node/Go
/// loggers emit, and none of the eight format parsers matches it. Every such
/// line was therefore stamped with *ingest* time, so `--last 1h` matched
/// events years old and `errors` reported when the file was read rather than
/// when the failure happened.
#[test]
fn test_leading_timestamp_on_unstructured_lines_is_the_event_time() {
    let (temp, log_file) = setup_project();
    append(
        &log_file,
        "2020-01-01T00:00:00Z ERROR ancient plaintext failure\n\
         [2020-01-01 00:00:05] ERROR bracketed ancient failure\n\
         2020-01-01T00:00:09+05:30 ERROR offset-aware ancient failure\n\
         2020-01-01T00:00:11+0000 ERROR colonless-offset ancient failure\n",
    );

    let recent = query(
        temp.path(),
        &["--quiet", "search", "--level", "error", "--last", "1h"],
    );
    assert_eq!(
        recent["meta"]["total"], 0,
        "nothing here happened in the last hour; ingest time must not stand in \
         for event time"
    );

    let all = query(
        temp.path(),
        &[
            "--quiet",
            "search",
            "--level",
            "error",
            "--start",
            "2019-01-01T00:00:00Z",
            "-n",
            "10",
        ],
    );
    let mut stamps: Vec<&str> = all["data"]["results"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|e| e["timestamp"].as_str())
        .collect();
    stamps.sort_unstable();
    assert_eq!(
        stamps,
        vec![
            // 00:00:09 +05:30 is 18:30:09 UTC the previous day.
            "2019-12-31T18:30:09.000Z",
            "2020-01-01T00:00:00.000Z",
            "2020-01-01T00:00:05.000Z",
            "2020-01-01T00:00:11.000Z",
        ],
        "each line's own timestamp, normalized to UTC"
    );
}

/// When a timestamp genuinely cannot be recovered, ingest time is a
/// reasonable stand-in — but the entry has to admit it, or a caller reading
/// `--last 1h` has no way to know the time was manufactured at read time.
#[test]
fn test_entries_dated_by_ingest_time_are_marked_as_such() {
    let (temp, log_file) = setup_project();
    append(&log_file, "widget factory failed, no timestamp anywhere\n");

    let value = query(
        temp.path(),
        &["--quiet", "search", "--level", "error", "-n", "5"],
    );
    let entry = &value["data"]["results"][0];
    assert_eq!(
        entry["attributes"]["timestamp_source"], "ingest",
        "a synthetic timestamp must be visible in the entry"
    );
}

/// ...and an entry that carries a real timestamp must not be marked.
#[test]
fn test_parsed_timestamps_carry_no_ingest_marker() {
    let (temp, log_file) = setup_project();
    append(
        &log_file,
        "{\"time\":\"2026-08-10T09:00:00Z\",\"level\":\"error\",\"msg\":\"dated\"}\n",
    );

    let value = query(
        temp.path(),
        &["--quiet", "search", "--level", "error", "-n", "5"],
    );
    let entry = &value["data"]["results"][0];
    assert!(
        entry["attributes"]["timestamp_source"].is_null(),
        "a parsed timestamp needs no marker, got {}",
        entry["attributes"]
    );
}
