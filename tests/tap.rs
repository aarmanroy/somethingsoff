//! Integration tests for the tap command: passthrough fidelity, journal
//! capture, inline indexing, and lock-contention degradation.

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

const LINES: &str = concat!(
    r#"{"timestamp": "2026-07-15T10:00:00.000Z", "level": "info", "message": "server listening on 3000"}"#,
    "\n",
    "plain text line that will not parse\n",
    r#"{"timestamp": "2026-07-15T10:00:01.000Z", "level": "error", "message": "unhandled rejection: boom"}"#,
    "\n",
);

#[test]
fn test_tap_passthrough_is_byte_identical() {
    let temp = TempDir::new().unwrap();
    let output = somethingsoff(temp.path())
        .args(["tap", "--source", "dev"])
        .write_stdin(LINES)
        .output()
        .unwrap();
    assert!(output.status.success());
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        LINES,
        "stdout must be an exact passthrough of stdin"
    );
}

#[test]
fn test_tap_journals_and_indexes() {
    let temp = TempDir::new().unwrap();
    let output = somethingsoff(temp.path())
        .args(["tap", "--source", "dev"])
        .write_stdin(LINES)
        .output()
        .unwrap();
    assert!(output.status.success());

    // Journal exists with the raw lines.
    let journal = temp.path().join(".somethingsoff/streams/dev.jsonl");
    assert_eq!(fs::read_to_string(&journal).unwrap(), LINES);

    // Summary envelope on stderr.
    let stderr = String::from_utf8_lossy(&output.stderr);
    let summary_line = stderr
        .lines()
        .find(|l| l.trim_start().starts_with('{'))
        .expect("summary envelope on stderr");
    let summary: serde_json::Value = serde_json::from_str(summary_line).unwrap();
    assert_eq!(summary["command"], "tap");
    assert_eq!(summary["data"]["lines"], 3);
    // All three lines are captured now: two JSON entries plus the plain-text
    // line as a raw entry. Ingest is lossless, so nothing "fails".
    assert_eq!(summary["data"]["entries_indexed"], 3);
    assert_eq!(summary["data"]["entries_failed"], 0);
    assert_eq!(summary["data"]["indexed_inline"], true);

    // Entries are searchable afterwards.
    let search = somethingsoff(temp.path())
        .args(["search", "--query", "\"unhandled rejection\""])
        .output()
        .unwrap();
    let parsed: serde_json::Value =
        serde_json::from_str(String::from_utf8_lossy(&search.stdout).trim()).unwrap();
    assert_eq!(parsed["meta"]["total"], 1);

    // The unstructured plain-text line is captured and searchable too.
    let raw_search = somethingsoff(temp.path())
        .args(["search", "--query", "\"will not parse\""])
        .output()
        .unwrap();
    let raw_parsed: serde_json::Value =
        serde_json::from_str(String::from_utf8_lossy(&raw_search.stdout).trim()).unwrap();
    assert_eq!(raw_parsed["meta"]["total"], 1);
    assert_eq!(
        raw_parsed["data"]["results"][0]["message"],
        "plain text line that will not parse"
    );

    // The journal cursor is registered: the follow-up search's sync must
    // not have re-ingested the journal (fresh fast path).
    assert_eq!(parsed["sync"]["ingested"], 0);
}

#[test]
fn test_tap_degrades_to_journal_only_when_locked() {
    let temp = TempDir::new().unwrap();
    let base_dir = temp.path().join(".somethingsoff");
    fs::create_dir_all(&base_dir).unwrap();

    // Hold the writer lock like a running `watch` would.
    let lock_file = fs::File::options()
        .create(true)
        .truncate(false)
        .write(true)
        .open(base_dir.join(".lock"))
        .unwrap();
    fs2::FileExt::try_lock_exclusive(&lock_file).unwrap();

    let output = somethingsoff(temp.path())
        .args(["tap", "--source", "dev"])
        .write_stdin(LINES)
        .output()
        .unwrap();
    assert!(output.status.success());

    let stderr = String::from_utf8_lossy(&output.stderr);
    let summary_line = stderr
        .lines()
        .find(|l| l.trim_start().starts_with('{'))
        .expect("summary envelope on stderr");
    let summary: serde_json::Value = serde_json::from_str(summary_line).unwrap();
    assert_eq!(summary["data"]["indexed_inline"], false);
    assert_eq!(summary["data"]["lines"], 3);

    // Journal captured everything even without the lock.
    let journal = temp.path().join(".somethingsoff/streams/dev.jsonl");
    assert_eq!(fs::read_to_string(&journal).unwrap(), LINES);

    // Release the lock; the journal is a discovered source, so a plain
    // search now ingests it.
    fs2::FileExt::unlock(&lock_file).unwrap();
    let search = somethingsoff(temp.path())
        .args(["search", "--query", "\"unhandled rejection\""])
        .output()
        .unwrap();
    let parsed: serde_json::Value =
        serde_json::from_str(String::from_utf8_lossy(&search.stdout).trim()).unwrap();
    assert_eq!(parsed["meta"]["total"], 1);
}
