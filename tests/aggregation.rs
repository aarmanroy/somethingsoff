use assert_cmd::Command;
use chrono::Utc;
use predicates::prelude::*;
use std::fs::File;
use std::io::Write;
use tempfile::TempDir;

/// Generate an ISO8601 timestamp string for N minutes ago from now
fn minutes_ago(minutes: i64) -> String {
    Utc::now()
        .checked_sub_signed(chrono::Duration::minutes(minutes))
        .unwrap()
        .format("%Y-%m-%dT%H:%M:%SZ")
        .to_string()
}

#[test]
fn test_stats_aggregation() {
    let temp_dir = TempDir::new().unwrap();
    let base_dir = temp_dir.path();
    let log_file_path = base_dir.join("stats.log");

    let ts1 = minutes_ago(10);
    let ts2 = minutes_ago(9);
    let ts3 = minutes_ago(8);
    let ts4 = minutes_ago(7);

    // 1. Create a log file with various entries
    let mut f = File::create(&log_file_path).unwrap();
    // info, route1, user1
    writeln!(f, r#"{{"timestamp": "{}", "level": "info", "message": "msg1", "route": "/api/v1/test", "user_id": "user1"}}"#, ts1).unwrap();
    // info, route1, user2
    writeln!(f, r#"{{"timestamp": "{}", "level": "info", "message": "msg2", "route": "/api/v1/test", "user_id": "user2"}}"#, ts2).unwrap();
    // error, route2, user1
    writeln!(f, r#"{{"timestamp": "{}", "level": "error", "message": "err1", "route": "/api/v2/data", "user_id": "user1"}}"#, ts3).unwrap();
    // warn, no route, no user
    writeln!(
        f,
        r#"{{"timestamp": "{}", "level": "warn", "message": "warn1"}}"#,
        ts4
    )
    .unwrap();
    // 2. Ingest
    let mut cmd = Command::cargo_bin("somethingsoff").unwrap();
    cmd.env("SOMETHINGSOFF_BASE_DIR", base_dir)
        .arg("ingest")
        .arg("--source")
        .arg("stats-source")
        .arg("--file")
        .arg(&log_file_path)
        .assert()
        .success();

    // 3. Test stats --by-level
    let mut cmd = Command::cargo_bin("somethingsoff").unwrap();
    cmd.env("SOMETHINGSOFF_BASE_DIR", base_dir)
        .arg("stats")
        .arg("--by-level")
        .assert()
        .success()
        .stdout(predicate::str::contains("\"info\":2"))
        .stdout(predicate::str::contains("\"error\":1"))
        .stdout(predicate::str::contains("\"warn\":1"));

    // 4. Test stats --by-route
    let mut cmd = Command::cargo_bin("somethingsoff").unwrap();
    cmd.env("SOMETHINGSOFF_BASE_DIR", base_dir)
        .arg("stats")
        .arg("--by-route")
        .assert()
        .success()
        .stdout(predicate::str::contains("\"/api/v1/test\":2"))
        .stdout(predicate::str::contains("\"/api/v2/data\":1"))
        .stdout(predicate::str::contains("\"unknown\":1"));

    // 5. Test stats --by-user
    let mut cmd = Command::cargo_bin("somethingsoff").unwrap();
    cmd.env("SOMETHINGSOFF_BASE_DIR", base_dir)
        .arg("stats")
        .arg("--by-user")
        .assert()
        .success()
        .stdout(predicate::str::contains("\"user1\":2"))
        .stdout(predicate::str::contains("\"user2\":1"))
        .stdout(predicate::str::contains("\"anonymous\":1"));

    // 6. Test stats --errors-only
    let mut cmd = Command::cargo_bin("somethingsoff").unwrap();
    cmd.env("SOMETHINGSOFF_BASE_DIR", base_dir)
        .arg("stats")
        .arg("--errors-only")
        .arg("--by-level")
        .assert()
        .success()
        .stdout(predicate::str::contains("\"total_logs\":1"))
        .stdout(predicate::str::contains("\"error\":1"))
        .stdout(predicate::str::contains("\"info\"").not());
}

#[test]
fn test_errors_aggregation() {
    let temp_dir = TempDir::new().unwrap();
    let base_dir = temp_dir.path();
    let log_file_path = base_dir.join("errors.log");

    let ts_a1 = minutes_ago(5);
    let ts_a2 = minutes_ago(4);
    let ts_a3 = minutes_ago(3);
    let ts_b1 = minutes_ago(2);
    let ts_b2 = minutes_ago(1);
    let ts_c = minutes_ago(1);

    // 1. Create a log file with various error entries
    let mut f = File::create(&log_file_path).unwrap();
    // Error A - 3 times
    writeln!(f, r#"{{"timestamp": "{}", "level": "error", "message": "Failed to connect to DB", "error": {{"name": "DatabaseError", "message": "Connection timeout"}}, "user_id": "user-1"}}"#, ts_a1).unwrap();
    writeln!(f, r#"{{"timestamp": "{}", "level": "error", "message": "Failed to connect to DB", "error": {{"name": "DatabaseError", "message": "Connection timeout"}}, "user_id": "user-2"}}"#, ts_a2).unwrap();
    writeln!(f, r#"{{"timestamp": "{}", "level": "error", "message": "Failed to connect to DB", "error": {{"name": "DatabaseError", "message": "Connection timeout"}}, "user_id": "user-3"}}"#, ts_a3).unwrap();
    // Error B - 2 times
    writeln!(f, r#"{{"timestamp": "{}", "level": "error", "message": "Auth failed", "error": {{"name": "AuthError", "message": "Invalid token"}}, "user_id": "user-1"}}"#, ts_b1).unwrap();
    writeln!(f, r#"{{"timestamp": "{}", "level": "error", "message": "Auth failed", "error": {{"name": "AuthError", "message": "Invalid token"}}, "user_id": "user-1"}}"#, ts_b2).unwrap();
    // Error C - 1 time (no structured error, fallback to message)
    writeln!(
        f,
        r#"{{"timestamp": "{}", "level": "error", "message": "Generic error occurred"}}"#,
        ts_c
    )
    .unwrap();

    // 2. Ingest
    let mut cmd = Command::cargo_bin("somethingsoff").unwrap();
    cmd.env("SOMETHINGSOFF_BASE_DIR", base_dir)
        .arg("ingest")
        .arg("--source")
        .arg("errors-source")
        .arg("--file")
        .arg(&log_file_path)
        .assert()
        .success();

    // 3. Test errors command
    let mut cmd = Command::cargo_bin("somethingsoff").unwrap();
    cmd.env("SOMETHINGSOFF_BASE_DIR", base_dir)
        .arg("errors")
        .arg("--last")
        .arg("24h")
        .assert()
        .success()
        .stdout(predicate::str::contains("\"total_errors\":6"))
        .stdout(predicate::str::contains("\"total_groups\":3"))
        // Check Error A
        .stdout(predicate::str::contains("\"error_name\":\"DatabaseError\""))
        .stdout(predicate::str::contains("\"count\":3"))
        .stdout(predicate::str::contains("\"affected_users\":3"))
        // Check Error B
        .stdout(predicate::str::contains("\"error_name\":\"AuthError\""))
        .stdout(predicate::str::contains("\"count\":2"))
        .stdout(predicate::str::contains("\"affected_users\":1"))
        // Check Error C
        .stdout(predicate::str::contains(
            "\"error_message\":\"Generic error occurred\"",
        ))
        .stdout(predicate::str::contains("\"count\":1"));
}
