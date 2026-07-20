use assert_cmd::Command;
use predicates::prelude::*;
use std::fs::File;
use std::io::Write;
use tempfile::TempDir;

#[test]
fn test_full_ingest_search_workflow() {
    let temp_dir = TempDir::new().unwrap();
    let base_dir = temp_dir.path();
    let log_file_path = base_dir.join("test.log");

    // 1. Create a dummy log file
    let mut f = File::create(&log_file_path).unwrap();
    writeln!(f, r#"{{"timestamp": "2026-03-22T10:00:00Z", "level": "info", "message": "Test message 1", "request_id": "req-1"}}"#).unwrap();
    writeln!(f, r#"{{"timestamp": "2026-03-22T10:05:00Z", "level": "error", "message": "Critical error 1", "request_id": "req-2", "user_id": "user-1"}}"#).unwrap();
    writeln!(f, r#"{{"timestamp": "2026-03-22T10:10:00Z", "level": "info", "message": "Test message 2", "request_id": "req-3"}}"#).unwrap();

    // 2. Ingest the log file
    let mut cmd = Command::cargo_bin("somethingsoff").unwrap();
    cmd.env("SOMETHINGSOFF_BASE_DIR", base_dir)
        .arg("ingest")
        .arg("--source")
        .arg("test-source")
        .arg("--file")
        .arg(&log_file_path)
        .assert()
        .success()
        .stdout(predicate::str::contains("\"ok\":true"))
        .stdout(predicate::str::contains("\"entries_indexed\":3"));

    // 3. Search for the error log
    let mut cmd = Command::cargo_bin("somethingsoff").unwrap();
    cmd.env("SOMETHINGSOFF_BASE_DIR", base_dir)
        .arg("search")
        .arg("--level")
        .arg("error")
        .assert()
        .success()
        .stdout(predicate::str::contains("Critical error 1"))
        .stdout(predicate::str::contains("req-2"))
        .stdout(predicate::str::contains("user-1"));

    // 4. Search with query
    let mut cmd = Command::cargo_bin("somethingsoff").unwrap();
    cmd.env("SOMETHINGSOFF_BASE_DIR", base_dir)
        .arg("search")
        .arg("--query")
        .arg("Critical")
        .assert()
        .success()
        .stdout(predicate::str::contains("Critical error 1"));

    // 5. Search for info logs
    let mut cmd = Command::cargo_bin("somethingsoff").unwrap();
    cmd.env("SOMETHINGSOFF_BASE_DIR", base_dir)
        .arg("search")
        .arg("--level")
        .arg("info")
        .assert()
        .success()
        .stdout(predicate::str::contains("Test message 1"))
        .stdout(predicate::str::contains("Test message 2"))
        .stdout(predicate::str::contains("req-1"))
        .stdout(predicate::str::contains("req-3"));
}

#[test]
fn test_ingest_positional_file_defaults_source_to_stem() {
    let temp_dir = TempDir::new().unwrap();
    let base_dir = temp_dir.path();
    let log_file_path = base_dir.join("Rclone-Hetzner.log");

    let mut f = File::create(&log_file_path).unwrap();
    writeln!(
        f,
        r#"{{"timestamp": "2026-03-22T10:00:00Z", "level": "info", "message": "Transferred 5 files"}}"#
    )
    .unwrap();

    // Positional FILE, no --source: source falls back to the lowercased stem.
    let mut cmd = Command::cargo_bin("somethingsoff").unwrap();
    cmd.env("SOMETHINGSOFF_BASE_DIR", base_dir)
        .arg("ingest")
        .arg(&log_file_path)
        .assert()
        .success()
        .stdout(predicate::str::contains("\"source\":\"rclone-hetzner\""))
        .stdout(predicate::str::contains("\"entries_indexed\":1"));

    // The derived source is searchable.
    let mut cmd = Command::cargo_bin("somethingsoff").unwrap();
    cmd.env("SOMETHINGSOFF_BASE_DIR", base_dir)
        .arg("search")
        .arg("--source")
        .arg("rclone-hetzner")
        .assert()
        .success()
        .stdout(predicate::str::contains("Transferred 5 files"));
}

#[test]
fn test_ingest_positional_and_flag_forms_conflict() {
    let temp_dir = TempDir::new().unwrap();
    let base_dir = temp_dir.path();
    let log_file_path = base_dir.join("app.log");
    File::create(&log_file_path).unwrap();

    // Both forms at once → usage error (exit 3, JSON envelope).
    let mut cmd = Command::cargo_bin("somethingsoff").unwrap();
    cmd.env("SOMETHINGSOFF_BASE_DIR", base_dir)
        .arg("ingest")
        .arg(&log_file_path)
        .arg("--file")
        .arg(&log_file_path)
        .assert()
        .code(3)
        .stdout(predicate::str::contains("\"code\":\"usage\""));

    // Neither form → usage error whose hint shows the ingest usage line.
    let mut cmd = Command::cargo_bin("somethingsoff").unwrap();
    cmd.env("SOMETHINGSOFF_BASE_DIR", base_dir)
        .arg("ingest")
        .assert()
        .code(3)
        .stdout(predicate::str::contains("Usage: somethingsoff ingest"));
}

#[test]
fn test_search_context_returns_surrounding_logs() {
    let temp_dir = TempDir::new().unwrap();
    let base_dir = temp_dir.path();
    let log_file_path = base_dir.join("context.log");

    let mut f = File::create(&log_file_path).unwrap();
    writeln!(
        f,
        r#"{{"timestamp": "2026-03-22T10:00:00Z", "level": "info", "message": "Booting service"}}"#
    )
    .unwrap();
    writeln!(f, r#"{{"timestamp": "2026-03-22T10:01:00Z", "level": "warn", "message": "Retrying database connection"}}"#).unwrap();
    writeln!(f, r#"{{"timestamp": "2026-03-22T10:02:00Z", "level": "error", "message": "Database connection lost"}}"#).unwrap();
    writeln!(f, r#"{{"timestamp": "2026-03-22T10:03:00Z", "level": "info", "message": "Starting graceful shutdown"}}"#).unwrap();

    let mut ingest_cmd = Command::cargo_bin("somethingsoff").unwrap();
    ingest_cmd
        .env("SOMETHINGSOFF_BASE_DIR", base_dir)
        .arg("ingest")
        .arg("--source")
        .arg("test-source")
        .arg("--file")
        .arg(&log_file_path);
    ingest_cmd.assert().success();

    let mut search_cmd = Command::cargo_bin("somethingsoff").unwrap();
    search_cmd
        .env("SOMETHINGSOFF_BASE_DIR", base_dir)
        .arg("search")
        .arg("--level")
        .arg("error")
        .arg("--limit")
        .arg("1")
        .arg("--context")
        .arg("1")
        .arg("--fields")
        .arg("message,level")
        .arg("--compact");

    search_cmd
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "\"target\":{\"level\":\"error\",\"message\":\"Database connection lost\"}",
        ))
        .stdout(predicate::str::contains(
            "\"before\":[{\"level\":\"warn\",\"message\":\"Retrying database connection\"}]",
        ))
        .stdout(predicate::str::contains(
            "\"after\":[{\"level\":\"info\",\"message\":\"Starting graceful shutdown\"}]",
        ))
        .stdout(predicate::str::contains("\"log_id\"").not())
        .stdout(predicate::str::contains("null").not());
}

#[test]
fn test_ingest_json_stack_trace_populates_source_location() {
    let temp_dir = TempDir::new().unwrap();
    let base_dir = temp_dir.path();
    let log_file_path = base_dir.join("stack.log");

    let mut f = File::create(&log_file_path).unwrap();
    writeln!(
        f,
        r#"{{"timestamp":"2026-03-22T10:05:00Z","level":"error","message":"Login failed","error":{{"name":"TypeError","message":"Cannot read property 'id'","stack":"TypeError: Cannot read property 'id'\n    at login (src/auth/login.ts:47:15)\n    at router (src/router.ts:23:10)"}}}}"#
    )
    .unwrap();

    let mut ingest_cmd = Command::cargo_bin("somethingsoff").unwrap();
    ingest_cmd
        .env("SOMETHINGSOFF_BASE_DIR", base_dir)
        .arg("ingest")
        .arg("--source")
        .arg("test-source")
        .arg("--file")
        .arg(&log_file_path);
    ingest_cmd.assert().success();

    let mut search_cmd = Command::cargo_bin("somethingsoff").unwrap();
    search_cmd
        .env("SOMETHINGSOFF_BASE_DIR", base_dir)
        .arg("search")
        .arg("--level")
        .arg("error")
        .arg("--fields")
        .arg("message,source_file,line_number")
        .arg("--compact");

    search_cmd
        .assert()
        .success()
        .stdout(predicate::str::contains("\"message\":\"Login failed\""))
        .stdout(predicate::str::contains(
            "\"source_file\":\"src/auth/login.ts\"",
        ))
        .stdout(predicate::str::contains("\"line_number\":47"));
}

#[test]
fn test_parse_format_stamp_stats_and_raw_filter() {
    let temp_dir = TempDir::new().unwrap();
    let base_dir = temp_dir.path();
    let log_file_path = base_dir.join("mixed.log");

    // Mixed corpus: 2 JSON, 1 logfmt, 2 unstructured lines.
    let mut f = File::create(&log_file_path).unwrap();
    writeln!(
        f,
        r#"{{"timestamp": "2026-03-22T10:00:00Z", "level": "info", "message": "json line one"}}"#
    )
    .unwrap();
    writeln!(
        f,
        r#"{{"timestamp": "2026-03-22T10:00:01Z", "level": "info", "message": "json line two"}}"#
    )
    .unwrap();
    writeln!(
        f,
        r#"time=2026-03-22T10:00:02Z level=info msg="logfmt line""#
    )
    .unwrap();
    writeln!(f, "plain build output line alpha").unwrap();
    writeln!(f, "plain build output line beta").unwrap();

    let mut ingest_cmd = Command::cargo_bin("somethingsoff").unwrap();
    ingest_cmd
        .env("SOMETHINGSOFF_BASE_DIR", base_dir)
        .arg("ingest")
        .arg("--source")
        .arg("mixed")
        .arg("--file")
        .arg(&log_file_path);
    ingest_cmd
        .assert()
        .success()
        .stdout(predicate::str::contains("\"entries_indexed\":5"));

    // stats --by-format reports the exact structured-vs-raw split.
    let mut stats_cmd = Command::cargo_bin("somethingsoff").unwrap();
    stats_cmd
        .env("SOMETHINGSOFF_BASE_DIR", base_dir)
        .args(["stats", "--by-format"]);
    stats_cmd
        .assert()
        .success()
        .stdout(predicate::str::contains("\"by_format\""))
        .stdout(predicate::str::contains("\"json\":2"))
        .stdout(predicate::str::contains("\"logfmt\":1"))
        .stdout(predicate::str::contains("\"raw\":2"));

    // --parse-format raw returns only the unparsed fallback entries.
    let mut raw_cmd = Command::cargo_bin("somethingsoff").unwrap();
    raw_cmd
        .env("SOMETHINGSOFF_BASE_DIR", base_dir)
        .args(["search", "--parse-format", "raw"]);
    raw_cmd
        .assert()
        .success()
        .stdout(predicate::str::contains("plain build output line alpha"))
        .stdout(predicate::str::contains("plain build output line beta"))
        .stdout(predicate::str::contains("\"parse_format\":\"raw\""))
        .stdout(predicate::str::contains("json line one").not())
        .stdout(predicate::str::contains("\"total\":2"));

    // Zero matches for a format not in the index → exit 2 (ok-but-empty).
    let mut none_cmd = Command::cargo_bin("somethingsoff").unwrap();
    none_cmd
        .env("SOMETHINGSOFF_BASE_DIR", base_dir)
        .args(["search", "--parse-format", "syslog"]);
    none_cmd.assert().code(2);
}

#[test]
fn test_stack_trace_is_one_searchable_event_and_errors_names_it() {
    let temp_dir = TempDir::new().unwrap();
    let base_dir = temp_dir.path();
    let log_file_path = base_dir.join("devserver.log");

    let mut f = File::create(&log_file_path).unwrap();
    writeln!(f, "Server listening on :3000").unwrap();
    writeln!(f, "TypeError: Cannot read property 'id' of undefined").unwrap();
    writeln!(f, "    at login (src/auth/login.ts:47:15)").unwrap();
    writeln!(f, "    at Router.handle (src/router.ts:23:10)").unwrap();
    writeln!(f, "GET /health 200").unwrap();

    let mut ingest_cmd = Command::cargo_bin("somethingsoff").unwrap();
    ingest_cmd
        .env("SOMETHINGSOFF_BASE_DIR", base_dir)
        .args(["ingest", "--source", "web", "--file"])
        .arg(&log_file_path);
    // 3 events: banner, coalesced stack trace, request line.
    ingest_cmd
        .assert()
        .success()
        .stdout(predicate::str::contains("\"entries_indexed\":3"));

    // The trace is ONE entry: anchor and frames in the same message.
    let mut search_cmd = Command::cargo_bin("somethingsoff").unwrap();
    search_cmd.env("SOMETHINGSOFF_BASE_DIR", base_dir).args([
        "search",
        "--level",
        "error",
        "--fields",
        "message,level",
        "--compact",
    ]);
    search_cmd
        .assert()
        .success()
        .stdout(predicate::str::contains("Cannot read property"))
        .stdout(predicate::str::contains(
            "at login (src/auth/login.ts:47:15)",
        ))
        .stdout(predicate::str::contains("\"total\":1"));

    // And `errors` names the actual failure as its top group.
    let mut errors_cmd = Command::cargo_bin("somethingsoff").unwrap();
    errors_cmd
        .env("SOMETHINGSOFF_BASE_DIR", base_dir)
        .args(["errors", "--last", "1h"]);
    errors_cmd
        .assert()
        .success()
        .stdout(predicate::str::contains("Cannot read property"))
        .stdout(predicate::str::contains("\"total_errors\":1"));
}
