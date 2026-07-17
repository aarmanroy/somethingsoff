use assert_cmd::Command;
use predicates::prelude::*;

#[test]
fn test_learn_command_suggests_bracketed_pattern() {
    let mut cmd = Command::cargo_bin("somethingsoff").unwrap();
    cmd.arg("learn")
        .arg("--sample")
        .arg("[2026-03-29] [DB] [ERROR] Timeout");

    cmd.assert()
        .success()
        .stdout(predicate::str::contains("\"ok\":true"))
        .stdout(predicate::str::contains("\"regex\":\"^\\\\[(?P<timestamp>"))
        .stdout(predicate::str::contains("\"timestamp\":\"2026-03-29\""))
        .stdout(predicate::str::contains("\"source\":\"DB\""))
        .stdout(predicate::str::contains("\"level\":\"ERROR\""))
        .stdout(predicate::str::contains("\"message\":\"Timeout\""));
}

#[test]
fn test_learn_command_suggests_delimited_pattern() {
    let mut cmd = Command::cargo_bin("somethingsoff").unwrap();
    cmd.arg("learn")
        .arg("--sample")
        .arg("2026-03-29 10:00:00,000 | WARN | api | timeout");

    cmd.assert()
        .success()
        .stdout(predicate::str::contains(
            "\"timestamp\":\"2026-03-29 10:00:00,000\"",
        ))
        .stdout(predicate::str::contains("\"level\":\"WARN\""))
        .stdout(predicate::str::contains("\"source\":\"api\""))
        .stdout(predicate::str::contains("\"message\":\"timeout\""));
}
