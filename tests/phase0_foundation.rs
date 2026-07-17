use assert_cmd::Command;
use predicates::prelude::*;
use std::fs;
use tempfile::TempDir;

#[test]
fn test_zero_config_mode() {
    let temp_dir = TempDir::new().expect("Failed to create temp dir");
    let logs_dir = temp_dir.path().join("logs");
    fs::create_dir(&logs_dir).unwrap();

    let log_file = logs_dir.join("test.log");
    fs::write(&log_file, r#"{"timestamp": "2026-03-30T05:00:00Z", "level": "info", "message": "test msg", "source": "test"}"#).unwrap();

    let mut cmd = Command::cargo_bin("somethingsoff").unwrap();
    cmd.current_dir(temp_dir.path())
        .env(
            "SOMETHINGSOFF_BASE_DIR",
            temp_dir.path().join(".somethingsoff"),
        )
        .arg("schema");

    // Zero config: ./logs/test.log is discovered AND auto-ingested before
    // the stats are computed — no ingest/rebuild step required.
    cmd.assert()
        .success()
        .stdout(predicate::str::contains("\"total_entries\":1"));
}

#[test]
fn test_zero_config_search_just_works() {
    // The core "just works" promise: a fresh project with a ./logs dir is
    // searchable with a single command — no ingest, no index rebuild.
    let temp_dir = TempDir::new().expect("Failed to create temp dir");
    let logs_dir = temp_dir.path().join("logs");
    fs::create_dir(&logs_dir).unwrap();

    let log_file = logs_dir.join("app.log");
    fs::write(&log_file, r#"{"timestamp": "2026-03-30T05:00:00Z", "level": "error", "message": "connection refused", "source": "app"}"#).unwrap();

    let mut cmd = Command::cargo_bin("somethingsoff").unwrap();
    cmd.current_dir(temp_dir.path())
        .env(
            "SOMETHINGSOFF_BASE_DIR",
            temp_dir.path().join(".somethingsoff"),
        )
        .arg("search")
        .arg("--level")
        .arg("error");

    cmd.assert()
        .success()
        .stdout(predicate::str::contains("connection refused"));

    // Appended lines are visible to the very next read, no extra steps.
    let mut f = fs::OpenOptions::new().append(true).open(&log_file).unwrap();
    use std::io::Write;
    writeln!(f).unwrap();
    writeln!(f, r#"{{"timestamp": "2026-03-30T05:01:00Z", "level": "error", "message": "second failure", "source": "app"}}"#).unwrap();
    drop(f);

    let mut cmd = Command::cargo_bin("somethingsoff").unwrap();
    cmd.current_dir(temp_dir.path())
        .env(
            "SOMETHINGSOFF_BASE_DIR",
            temp_dir.path().join(".somethingsoff"),
        )
        .arg("search")
        .arg("--level")
        .arg("error");

    cmd.assert()
        .success()
        .stdout(predicate::str::contains("second failure"));
}

#[test]
fn test_schema_command() {
    let temp_dir = TempDir::new().expect("Failed to create temp dir");
    let base_dir = temp_dir.path().join(".somethingsoff");

    let mut cmd = Command::cargo_bin("somethingsoff").unwrap();
    cmd.env("SOMETHINGSOFF_BASE_DIR", &base_dir).arg("schema");

    cmd.assert()
        .success()
        .stdout(predicate::str::contains("\"ok\":true"))
        .stdout(predicate::str::contains("\"system_info\""))
        .stdout(predicate::str::contains("\"schema\""))
        .stdout(predicate::str::contains("\"fields\""));
}

#[test]
fn test_semantic_exit_codes() {
    let temp_dir = TempDir::new().expect("Failed to create temp dir");
    let base_dir = temp_dir.path().join(".somethingsoff");

    // 2 = Success, no results
    let mut cmd = Command::cargo_bin("somethingsoff").unwrap();
    cmd.env("SOMETHINGSOFF_BASE_DIR", &base_dir)
        .arg("search")
        .arg("--query")
        .arg("nonexistent");

    // Since index doesn't exist, it might fail with code 1 or 4
    // But let's first build an empty index
    let mut build_cmd = Command::cargo_bin("somethingsoff").unwrap();
    build_cmd
        .env("SOMETHINGSOFF_BASE_DIR", &base_dir)
        .arg("index")
        .arg("rebuild");
    build_cmd.assert().success();

    let mut cmd = Command::cargo_bin("somethingsoff").unwrap();
    cmd.env("SOMETHINGSOFF_BASE_DIR", &base_dir)
        .arg("search")
        .arg("--query")
        .arg("nonexistent");

    cmd.assert().code(2);
}

#[test]
fn test_compact_and_fields_output() {
    let temp_dir = TempDir::new().expect("Failed to create temp dir");
    let base_dir = temp_dir.path().join(".somethingsoff");
    let logs_dir = temp_dir.path().join("logs");
    fs::create_dir(&logs_dir).unwrap();

    let log_file = logs_dir.join("test.log");
    fs::write(&log_file, r#"{"timestamp": "2026-03-30T05:00:00Z", "level": "info", "message": "test msg", "source": "test", "user_id": "user1"}
{"timestamp": "2026-03-30T05:01:00Z", "level": "error", "message": "err msg", "source": "test", "request_id": "req1"}"#).unwrap();

    // Ingest
    let mut ingest_cmd = Command::cargo_bin("somethingsoff").unwrap();
    ingest_cmd
        .env("SOMETHINGSOFF_BASE_DIR", &base_dir)
        .arg("ingest")
        .arg("--source")
        .arg("test")
        .arg("--file")
        .arg(&log_file);
    ingest_cmd.assert().success();

    // Search with --fields
    let mut search_cmd = Command::cargo_bin("somethingsoff").unwrap();
    search_cmd
        .env("SOMETHINGSOFF_BASE_DIR", &base_dir)
        .arg("search")
        .arg("--query")
        .arg("test")
        .arg("--fields")
        .arg("message,level");

    search_cmd
        .assert()
        .success()
        .stdout(predicate::str::contains("\"message\":\"test msg\""))
        .stdout(predicate::str::contains("\"level\":\"info\""))
        .stdout(predicate::str::contains("\"log_id\"").not());

    // Search with --compact
    let mut compact_cmd = Command::cargo_bin("somethingsoff").unwrap();
    compact_cmd
        .env("SOMETHINGSOFF_BASE_DIR", &base_dir)
        .arg("search")
        .arg("--query")
        .arg("test")
        .arg("--compact");

    compact_cmd
        .assert()
        .success()
        .stdout(predicate::str::contains("null").not());
}
