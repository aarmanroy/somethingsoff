//! Integration tests for the auto-ingest-on-query sync engine.
//!
//! The core "just works" contract: read commands transparently discover
//! sources, create the index, and ingest new bytes before answering.

use assert_cmd::Command;
use predicates::prelude::*;
use std::fs;
use std::io::Write;
use std::path::Path;
use tempfile::TempDir;

fn write_line(path: &Path, line: &str) {
    let mut f = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .unwrap();
    writeln!(f, "{}", line).unwrap();
}

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

#[test]
fn test_fresh_project_search_works_immediately() {
    let (temp, log_file) = setup_project();
    write_line(
        &log_file,
        r#"{"timestamp": "2026-03-30T05:00:00.000Z", "level": "error", "message": "boom"}"#,
    );

    somethingsoff(temp.path())
        .args(["search", "--level", "error"])
        .assert()
        .success()
        .stdout(predicate::str::contains("boom"));
}

#[test]
fn test_appends_visible_to_next_read() {
    let (temp, log_file) = setup_project();
    write_line(
        &log_file,
        r#"{"timestamp": "2026-03-30T05:00:00.000Z", "level": "info", "message": "first"}"#,
    );

    somethingsoff(temp.path())
        .args(["search", "--query", "first"])
        .assert()
        .success();

    write_line(
        &log_file,
        r#"{"timestamp": "2026-03-30T05:01:00.000Z", "level": "info", "message": "second"}"#,
    );

    somethingsoff(temp.path())
        .args(["search", "--query", "second"])
        .assert()
        .success()
        .stdout(predicate::str::contains("\"total\":1"));
}

#[test]
fn test_truncate_and_rewrite_triggers_rotation_reingest() {
    let (temp, log_file) = setup_project();
    write_line(
        &log_file,
        r#"{"timestamp": "2026-03-30T05:00:00.000Z", "level": "info", "message": "old content"}"#,
    );

    somethingsoff(temp.path())
        .args(["stats"])
        .assert()
        .success();

    // Simulate copytruncate rotation: truncate, then write fresh content.
    fs::write(&log_file, "").unwrap();
    write_line(
        &log_file,
        r#"{"timestamp": "2026-03-30T06:00:00.000Z", "level": "info", "message": "after rotation"}"#,
    );

    somethingsoff(temp.path())
        .args(["search", "--query", "rotation"])
        .assert()
        .success()
        .stdout(predicate::str::contains("after rotation"));
}

#[test]
fn test_corrupt_state_still_returns_correct_results() {
    let (temp, log_file) = setup_project();
    write_line(
        &log_file,
        r#"{"timestamp": "2026-03-30T05:00:00.000Z", "level": "error", "message": "resilient"}"#,
    );

    somethingsoff(temp.path())
        .args(["stats"])
        .assert()
        .success();

    // Corrupt the state file: the next read re-ingests from scratch and
    // dedup keeps the results correct (no duplicates).
    fs::write(temp.path().join(".somethingsoff/state.json"), "{corrupt!").unwrap();

    somethingsoff(temp.path())
        .args(["search", "--query", "resilient"])
        .assert()
        .success()
        .stdout(predicate::str::contains("\"total\":1"));
}

#[test]
fn test_no_sync_flag_skips_ingestion() {
    let (temp, log_file) = setup_project();
    write_line(
        &log_file,
        r#"{"timestamp": "2026-03-30T05:00:00.000Z", "level": "error", "message": "invisible"}"#,
    );

    // With --no-sync on a fresh project the (empty) index is still created,
    // but nothing is ingested: zero results, exit code 2.
    somethingsoff(temp.path())
        .args(["--no-sync", "search", "--level", "error"])
        .assert()
        .code(2);

    // Without the flag the same query finds the entry.
    somethingsoff(temp.path())
        .args(["search", "--level", "error"])
        .assert()
        .success()
        .stdout(predicate::str::contains("invisible"));
}

#[test]
fn test_new_file_discovered_after_first_sync() {
    let (temp, log_file) = setup_project();
    write_line(
        &log_file,
        r#"{"timestamp": "2026-03-30T05:00:00.000Z", "level": "info", "message": "from app"}"#,
    );

    somethingsoff(temp.path())
        .args(["stats"])
        .assert()
        .success();

    // A brand-new file appearing in ./logs is picked up by the next read.
    let other = temp.path().join("logs/worker.log");
    write_line(
        &other,
        r#"{"timestamp": "2026-03-30T05:02:00.000Z", "level": "warn", "message": "from worker"}"#,
    );

    somethingsoff(temp.path())
        .args(["search", "--query", "worker"])
        .assert()
        .success()
        .stdout(predicate::str::contains("from worker"));
}

#[test]
fn test_explicit_ingest_file_stays_fresh_afterwards() {
    let temp = TempDir::new().unwrap();
    // Log file OUTSIDE ./logs — only known via explicit ingest.
    let log_file = temp.path().join("custom.log");
    write_line(
        &log_file,
        r#"{"timestamp": "2026-03-30T05:00:00.000Z", "level": "info", "message": "explicit"}"#,
    );

    somethingsoff(temp.path())
        .args(["ingest", "--source", "custom", "--file"])
        .arg(&log_file)
        .assert()
        .success();

    // Appends to the explicitly-ingested file are auto-synced on read.
    write_line(
        &log_file,
        r#"{"timestamp": "2026-03-30T05:03:00.000Z", "level": "info", "message": "explicit-append"}"#,
    );

    somethingsoff(temp.path())
        .args(["search", "--query", "\"explicit-append\""])
        .assert()
        .success()
        .stdout(predicate::str::contains("explicit-append"));
}
