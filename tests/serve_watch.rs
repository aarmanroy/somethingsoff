use assert_cmd::prelude::*;
use assert_cmd::Command;
use predicates::prelude::*;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::process::{Child, Stdio};
use std::thread;
use std::time::Duration;
use tempfile::TempDir;

struct KillOnDrop(Child);
impl Drop for KillOnDrop {
    fn drop(&mut self) {
        let _ = self.0.kill();
    }
}

#[test]
fn test_watch_mode_ingestion() {
    let temp_dir = TempDir::new().unwrap();
    let base_dir = temp_dir.path();
    let log_file_path = base_dir.join("watch.log");

    // 1. Create initial log file with 1 entry
    {
        let mut f = File::create(&log_file_path).unwrap();
        writeln!(
            f,
            r#"{{"timestamp": "2026-03-22T12:00:00Z", "level": "INFO", "message": "initial"}}"#
        )
        .unwrap();
    }

    // 2. Start serve --watch in background
    // We use a short interval (1s) for testing
    let mut cmd = std::process::Command::cargo_bin("somethingsoff").unwrap();
    cmd.env("SOMETHINGSOFF_BASE_DIR", base_dir)
        .arg("serve")
        .arg("--watch")
        .arg("--interval")
        .arg("1")
        .arg("--source")
        .arg("watch-source")
        .arg("--file")
        .arg(&log_file_path)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let child = cmd.spawn().expect("Failed to start watch mode");
    let _killer = KillOnDrop(child);

    // Wait for initial ingestion
    thread::sleep(Duration::from_secs(2));

    // 3. Verify initial entry is indexed
    let mut cmd = Command::cargo_bin("somethingsoff").unwrap();
    cmd.env("SOMETHINGSOFF_BASE_DIR", base_dir)
        .arg("search")
        .arg("--query")
        .arg("initial")
        .assert()
        .success()
        .stdout(predicate::str::contains("initial"));

    // 4. Append a new entry
    {
        let mut f = OpenOptions::new()
            .append(true)
            .open(&log_file_path)
            .unwrap();
        writeln!(
            f,
            r#"{{"timestamp": "2026-03-22T12:05:00Z", "level": "INFO", "message": "second"}}"#
        )
        .unwrap();
    }

    // Wait for auto-ingestion
    thread::sleep(Duration::from_secs(2));

    // 5. Verify second entry is indexed
    let mut cmd = Command::cargo_bin("somethingsoff").unwrap();
    cmd.env("SOMETHINGSOFF_BASE_DIR", base_dir)
        .arg("search")
        .arg("--query")
        .arg("second")
        .assert()
        .success()
        .stdout(predicate::str::contains("second"));

    // 6. Append another entry
    {
        let mut f = OpenOptions::new()
            .append(true)
            .open(&log_file_path)
            .unwrap();
        writeln!(f, r#"{{"timestamp": "2026-03-22T12:10:00Z", "level": "ERROR", "message": "third", "error": {{"name": "WatchError", "message": "Watch failed"}}}}"#).unwrap();
    }

    // Wait for auto-ingestion
    thread::sleep(Duration::from_secs(2));

    // 7. Verify third entry is indexed
    let mut cmd = Command::cargo_bin("somethingsoff").unwrap();
    cmd.env("SOMETHINGSOFF_BASE_DIR", base_dir)
        .arg("search")
        .arg("--query")
        .arg("third")
        .assert()
        .success()
        .stdout(predicate::str::contains("third"))
        .stdout(predicate::str::contains("WatchError"));
}

#[test]
fn test_watch_mode_log_rotation() {
    let temp_dir = TempDir::new().unwrap();
    let base_dir = temp_dir.path();
    let log_file_path = base_dir.join("rotation.log");

    // 1. Create initial log file with entries
    {
        let mut f = File::create(&log_file_path).unwrap();
        writeln!(
            f,
            r#"{{"timestamp": "2026-03-22T12:00:00Z", "level": "INFO", "message": "before-rotation"}}"#
        )
        .unwrap();
        writeln!(
            f,
            r#"{{"timestamp": "2026-03-22T12:01:00Z", "level": "INFO", "message": "also-before"}}"#
        )
        .unwrap();
    }

    // 2. Start serve --watch in background
    let mut cmd = std::process::Command::cargo_bin("somethingsoff").unwrap();
    cmd.env("SOMETHINGSOFF_BASE_DIR", base_dir)
        .arg("serve")
        .arg("--watch")
        .arg("--interval")
        .arg("1")
        .arg("--source")
        .arg("rotation-source")
        .arg("--file")
        .arg(&log_file_path)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let child = cmd.spawn().expect("Failed to start watch mode");
    let _killer = KillOnDrop(child);

    // Wait for initial ingestion
    thread::sleep(Duration::from_secs(2));

    // 3. Verify initial entries are indexed
    let mut cmd = Command::cargo_bin("somethingsoff").unwrap();
    cmd.env("SOMETHINGSOFF_BASE_DIR", base_dir)
        .arg("search")
        .arg("--query")
        .arg("before-rotation")
        .assert()
        .success()
        .stdout(predicate::str::contains("before-rotation"));

    // 4. Simulate log rotation: truncate file and write new entry
    {
        let mut f = File::create(&log_file_path).unwrap(); // Truncate
        writeln!(
            f,
            r#"{{"timestamp": "2026-03-22T12:05:00Z", "level": "INFO", "message": "after-rotation"}}"#
        )
        .unwrap();
    }

    // Wait for auto-ingestion and rotation detection
    thread::sleep(Duration::from_secs(3));

    // 5. Verify new entry is indexed (rotation was detected)
    let mut cmd = Command::cargo_bin("somethingsoff").unwrap();
    cmd.env("SOMETHINGSOFF_BASE_DIR", base_dir)
        .arg("search")
        .arg("--query")
        .arg("after-rotation")
        .assert()
        .success()
        .stdout(predicate::str::contains("after-rotation"));
}
