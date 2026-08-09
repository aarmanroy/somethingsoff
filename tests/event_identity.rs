//! Event identity: what counts as "the same log entry".
//!
//! An entry's `log_id` has to answer two questions at once, and getting
//! either wrong loses data silently:
//!
//! 1. Re-reading the same bytes (a replay, a rotated file, a rebuilt index)
//!    must collapse — otherwise every restart inflates the counts.
//! 2. Two separate occurrences must NOT collapse — otherwise an outage that
//!    emits the same line 200 times reports it once.
//!
//! Content-only identity satisfied (1) and failed (2) catastrophically: every
//! second-resolution format (Apache, nginx, syslog, Rails) renders repeated
//! events byte-identical, so 200 failures indexed as 4. Path+offset identity
//! satisfied (2) and failed (1) on rotation. Identity is keyed on
//! `source + byte offset`, which satisfies both.

use assert_cmd::Command;
use std::fs;
use std::path::Path;
use tempfile::TempDir;

fn somethingsoff(project_dir: &Path) -> Command {
    let mut cmd = Command::cargo_bin("somethingsoff").unwrap();
    cmd.current_dir(project_dir)
        .env("SOMETHINGSOFF_BASE_DIR", project_dir.join(".somethingsoff"));
    cmd
}

/// Total documents currently reachable in the index.
fn indexed_count(project: &Path) -> u64 {
    let out = somethingsoff(project)
        .args(["--quiet", "stats"])
        .assert()
        .success();
    let stdout = String::from_utf8(out.get_output().stdout.clone()).unwrap();
    let value: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    value["data"]["total_logs"].as_u64().unwrap()
}

fn project_with_logs() -> TempDir {
    let temp = TempDir::new().unwrap();
    fs::create_dir(temp.path().join("logs")).unwrap();
    temp
}

/// An outage emits the same line many times within one second. Apache stamps
/// at second resolution, so all of them render identically — the case that
/// collapsed 200 events into 4.
#[test]
fn test_repeated_identical_events_keep_honest_counts() {
    let temp = project_with_logs();
    let mut body = String::new();
    for second in 0..4 {
        for _ in 0..50 {
            body.push_str(&format!(
                "10.0.0.1 - - [10/Aug/2026:09:00:{second:02} +0000] \"GET /checkout HTTP/1.1\" 500 128\n"
            ));
        }
    }
    fs::write(temp.path().join("logs/access.log"), body).unwrap();

    assert_eq!(
        indexed_count(temp.path()),
        200,
        "every request is a real event; second-resolution timestamps must not \
         merge them"
    );
}

/// Distinct requests that happen to render alike must not overwrite each
/// other. Here a genuine 500 sat between a 200 and a 503 and disappeared.
#[test]
fn test_distinct_requests_in_the_same_second_all_survive() {
    let temp = project_with_logs();
    fs::write(
        temp.path().join("logs/access.log"),
        "10.0.0.1 - - [10/Aug/2026:09:00:00 +0000] \"GET /checkout HTTP/1.1\" 200 128\n\
         10.0.0.2 - - [10/Aug/2026:09:00:00 +0000] \"GET /checkout HTTP/1.1\" 500 64\n\
         10.0.0.3 - - [10/Aug/2026:09:00:00 +0000] \"GET /checkout HTTP/1.1\" 503 12\n",
    )
    .unwrap();

    let out = somethingsoff(temp.path())
        .args(["--quiet", "search", "-n", "50"])
        .assert()
        .success();
    let stdout = String::from_utf8(out.get_output().stdout.clone()).unwrap();
    let value: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    let mut statuses: Vec<u64> = value["data"]["results"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|e| e["status_code"].as_u64())
        .collect();
    statuses.sort_unstable();

    assert_eq!(
        statuses,
        vec![200, 500, 503],
        "three different requests must remain three entries"
    );
}

/// Move-and-recreate rotation re-reads the rotated file from offset 0 under a
/// new filename. Because identity is keyed on the logical source rather than
/// the path, those bytes reproduce the ids the live file already wrote.
#[test]
fn test_rotation_does_not_duplicate_structured_entries() {
    let temp = project_with_logs();
    let live = temp.path().join("logs/app.log");
    fs::write(
        &live,
        "{\"time\":\"2026-08-10T09:00:01.000Z\",\"level\":\"error\",\"msg\":\"one\"}\n\
         {\"time\":\"2026-08-10T09:00:02.000Z\",\"level\":\"error\",\"msg\":\"two\"}\n\
         {\"time\":\"2026-08-10T09:00:03.000Z\",\"level\":\"error\",\"msg\":\"three\"}\n",
    )
    .unwrap();
    assert_eq!(indexed_count(temp.path()), 3);

    fs::rename(&live, temp.path().join("logs/app.log.1")).unwrap();
    fs::write(&live, "").unwrap();

    assert_eq!(
        indexed_count(temp.path()),
        3,
        "rotated content is the same three events, not three more"
    );
}

/// The same guarantee for lines no structured parser claims. These were
/// already position-salted, but keyed on the file path — so rotation
/// duplicated them, which nothing caught.
#[test]
fn test_rotation_does_not_duplicate_raw_entries() {
    let temp = project_with_logs();
    let live = temp.path().join("logs/app.log");
    fs::write(
        &live,
        "boot sequence started\nwidget factory warming up\nready to serve\n",
    )
    .unwrap();
    assert_eq!(indexed_count(temp.path()), 3);

    fs::rename(&live, temp.path().join("logs/app.log.1")).unwrap();
    fs::write(&live, "").unwrap();

    assert_eq!(
        indexed_count(temp.path()),
        3,
        "raw lines must dedup across rotation too"
    );
}

/// Appending must add exactly the new events, even when the appended text is
/// identical to what came before.
#[test]
fn test_appending_identical_lines_adds_them() {
    let temp = project_with_logs();
    let live = temp.path().join("logs/app.log");
    let line = "{\"time\":\"2026-08-10T09:00:00Z\",\"level\":\"error\",\"msg\":\"same\"}\n";

    fs::write(&live, line.repeat(3)).unwrap();
    assert_eq!(indexed_count(temp.path()), 3);

    fs::write(&live, line.repeat(5)).unwrap();
    assert_eq!(
        indexed_count(temp.path()),
        5,
        "two more occurrences were appended, so two more entries"
    );
}
