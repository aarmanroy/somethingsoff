use assert_cmd::Command;
use predicates::prelude::*;
use serde_json::Value;
use std::fs::File;
use std::io::Write;
use tempfile::TempDir;

#[test]
fn test_deduplication_on_duplicate_ingest() {
    let temp_dir = TempDir::new().unwrap();
    let base_dir = temp_dir.path();
    let log_file_path = base_dir.join("test.log");

    // 1. Create a log file with 3 entries
    let mut f = File::create(&log_file_path).unwrap();
    writeln!(
        f,
        r#"{{"timestamp": "2026-03-22T10:00:00Z", "level": "info", "message": "Dedup test A", "request_id": "req-1"}}"#
    )
    .unwrap();
    writeln!(
        f,
        r#"{{"timestamp": "2026-03-22T10:05:00Z", "level": "error", "message": "Dedup test B", "request_id": "req-2"}}"#
    )
    .unwrap();
    writeln!(
        f,
        r#"{{"timestamp": "2026-03-22T10:10:00Z", "level": "warn", "message": "Dedup test C", "request_id": "req-3"}}"#
    )
    .unwrap();

    // 2. Ingest for the first time — 3 new, 0 dedup
    let mut cmd = Command::cargo_bin("somethingsoff").unwrap();
    cmd.env("SOMETHINGSOFF_BASE_DIR", base_dir)
        .arg("ingest")
        .arg("--source")
        .arg("test-dedup")
        .arg("--file")
        .arg(&log_file_path)
        .assert()
        .success()
        .stdout(predicate::str::contains("\"entries_indexed\":3"))
        .stdout(predicate::str::contains("\"entries_deduplicated\":0"));

    // 3. Ingest the SAME file again — 3 upserted, 3 dedup (accurate count!)
    let mut cmd = Command::cargo_bin("somethingsoff").unwrap();
    cmd.env("SOMETHINGSOFF_BASE_DIR", base_dir)
        .arg("ingest")
        .arg("--source")
        .arg("test-dedup")
        .arg("--file")
        .arg(&log_file_path)
        .assert()
        .success()
        .stdout(predicate::str::contains("\"entries_indexed\":3"))
        .stdout(predicate::str::contains("\"entries_deduplicated\":3"));

    // 4. Search — exactly 3 unique results (not 6)
    let mut cmd = Command::cargo_bin("somethingsoff").unwrap();
    let output = cmd
        .env("SOMETHINGSOFF_BASE_DIR", base_dir)
        .arg("search")
        .arg("--query")
        .arg("Dedup test")
        .assert()
        .success()
        .get_output()
        .clone();

    let stdout = String::from_utf8_lossy(&output.stdout);
    let json: Value = serde_json::from_str(&stdout).expect("Search output should be valid JSON");
    let total_count = json["meta"]["total"]
        .as_u64()
        .expect("meta.total should be a number");
    assert_eq!(
        total_count, 3,
        "Should have exactly 3 unique entries after double ingest, got {total_count}"
    );
}

#[test]
fn test_deduplication_partial_overlap() {
    let temp_dir = TempDir::new().unwrap();
    let base_dir = temp_dir.path();
    let log_file_a = base_dir.join("file_a.log");
    let log_file_b = base_dir.join("file_b.log");

    // File A: 2 entries
    let mut f = File::create(&log_file_a).unwrap();
    writeln!(
        f,
        r#"{{"timestamp": "2026-03-22T10:00:00Z", "level": "info", "message": "Shared entry", "request_id": "req-shared"}}"#
    )
    .unwrap();
    writeln!(
        f,
        r#"{{"timestamp": "2026-03-22T10:05:00Z", "level": "error", "message": "Only in A", "request_id": "req-a"}}"#
    )
    .unwrap();

    // File B: 2 entries — one overlaps (same content → same log_id)
    let mut f = File::create(&log_file_b).unwrap();
    writeln!(
        f,
        r#"{{"timestamp": "2026-03-22T10:00:00Z", "level": "info", "message": "Shared entry", "request_id": "req-shared"}}"#
    )
    .unwrap();
    writeln!(
        f,
        r#"{{"timestamp": "2026-03-22T10:10:00Z", "level": "warn", "message": "Only in B", "request_id": "req-b"}}"#
    )
    .unwrap();

    // 1. Ingest file A — 2 new, 0 dedup
    let mut cmd = Command::cargo_bin("somethingsoff").unwrap();
    cmd.env("SOMETHINGSOFF_BASE_DIR", base_dir)
        .arg("ingest")
        .arg("--source")
        .arg("test-overlap")
        .arg("--file")
        .arg(&log_file_a)
        .assert()
        .success()
        .stdout(predicate::str::contains("\"entries_indexed\":2"))
        .stdout(predicate::str::contains("\"entries_deduplicated\":0"));

    // 2. Ingest file B — 2 indexed, 1 dedup (the shared entry)
    let mut cmd = Command::cargo_bin("somethingsoff").unwrap();
    cmd.env("SOMETHINGSOFF_BASE_DIR", base_dir)
        .arg("ingest")
        .arg("--source")
        .arg("test-overlap")
        .arg("--file")
        .arg(&log_file_b)
        .assert()
        .success()
        .stdout(predicate::str::contains("\"entries_indexed\":2"))
        .stdout(predicate::str::contains("\"entries_deduplicated\":1"));

    // 3. Search — exactly 3 unique
    let mut cmd = Command::cargo_bin("somethingsoff").unwrap();
    let output = cmd
        .env("SOMETHINGSOFF_BASE_DIR", base_dir)
        .arg("search")
        .arg("--query")
        .arg("entry OR Only")
        .assert()
        .success()
        .get_output()
        .clone();

    let stdout = String::from_utf8_lossy(&output.stdout);
    let json: Value = serde_json::from_str(&stdout).expect("Search output should be valid JSON");
    let total_count = json["meta"]["total"]
        .as_u64()
        .expect("meta.total should be a number");
    assert_eq!(
        total_count, 3,
        "Should have exactly 3 unique entries from overlapping files, got {total_count}"
    );
}

#[test]
fn test_deduplication_preserves_search_results() {
    let temp_dir = TempDir::new().unwrap();
    let base_dir = temp_dir.path();
    let log_file_path = base_dir.join("test.log");

    let mut f = File::create(&log_file_path).unwrap();
    writeln!(
        f,
        r#"{{"timestamp": "2026-03-22T10:00:00Z", "level": "error", "message": "Persistent error", "request_id": "req-1"}}"#
    )
    .unwrap();

    // Ingest twice
    let mut cmd = Command::cargo_bin("somethingsoff").unwrap();
    cmd.env("SOMETHINGSOFF_BASE_DIR", base_dir)
        .arg("ingest")
        .arg("--source")
        .arg("test-persist")
        .arg("--file")
        .arg(&log_file_path)
        .assert()
        .success();

    let mut cmd = Command::cargo_bin("somethingsoff").unwrap();
    cmd.env("SOMETHINGSOFF_BASE_DIR", base_dir)
        .arg("ingest")
        .arg("--source")
        .arg("test-persist")
        .arg("--file")
        .arg(&log_file_path)
        .assert()
        .success()
        .stdout(predicate::str::contains("\"entries_deduplicated\":1"));

    // Search — exactly 1 unique result
    let mut cmd = Command::cargo_bin("somethingsoff").unwrap();
    cmd.env("SOMETHINGSOFF_BASE_DIR", base_dir)
        .arg("search")
        .arg("--level")
        .arg("error")
        .assert()
        .success()
        .stdout(predicate::str::contains("Persistent error"))
        .stdout(predicate::str::contains("\"total\":1"));
}

#[test]
fn test_deduplication_within_batch() {
    let temp_dir = TempDir::new().unwrap();
    let base_dir = temp_dir.path();
    let log_file_path = base_dir.join("test.log");

    // 2 identical lines + 1 unique = 3 indexed, 1 deduped
    let mut f = File::create(&log_file_path).unwrap();
    writeln!(
        f,
        r#"{{"timestamp": "2026-03-22T10:00:00Z", "level": "info", "message": "Duplicate line", "request_id": "req-1"}}"#
    )
    .unwrap();
    writeln!(
        f,
        r#"{{"timestamp": "2026-03-22T10:00:00Z", "level": "info", "message": "Duplicate line", "request_id": "req-1"}}"#
    )
    .unwrap();
    writeln!(
        f,
        r#"{{"timestamp": "2026-03-22T10:05:00Z", "level": "warn", "message": "Unique line", "request_id": "req-2"}}"#
    )
    .unwrap();

    // Ingest — 3 indexed, 1 deduped (via count-based approach)
    let mut cmd = Command::cargo_bin("somethingsoff").unwrap();
    cmd.env("SOMETHINGSOFF_BASE_DIR", base_dir)
        .arg("ingest")
        .arg("--source")
        .arg("test-batch-dedup")
        .arg("--file")
        .arg(&log_file_path)
        .assert()
        .success()
        .stdout(predicate::str::contains("\"entries_indexed\":3"))
        .stdout(predicate::str::contains("\"entries_deduplicated\":1"));

    // Search — exactly 2 unique
    let mut cmd = Command::cargo_bin("somethingsoff").unwrap();
    let output = cmd
        .env("SOMETHINGSOFF_BASE_DIR", base_dir)
        .arg("search")
        .arg("--query")
        .arg("Duplicate OR Unique")
        .assert()
        .success()
        .get_output()
        .clone();

    let stdout = String::from_utf8_lossy(&output.stdout);
    let json: Value = serde_json::from_str(&stdout).expect("Search output should be valid JSON");
    let total_count = json["meta"]["total"]
        .as_u64()
        .expect("meta.total should be a number");
    assert_eq!(
        total_count, 2,
        "Should have exactly 2 unique entries (one deduped), got {total_count}"
    );
}
