//! v1 output-contract tests: every command emits the same envelope shape,
//! failures carry code/hint/exit_code, and exit codes follow the table in
//! `src/output.rs`.

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

fn setup_project_with_logs() -> TempDir {
    let temp = TempDir::new().unwrap();
    let logs_dir = temp.path().join("logs");
    fs::create_dir(&logs_dir).unwrap();
    fs::write(
        logs_dir.join("app.log"),
        concat!(
            r#"{"timestamp": "2026-03-30T05:00:00.000Z", "level": "info", "message": "started"}"#,
            "\n",
            r#"{"timestamp": "2026-03-30T05:01:00.000Z", "level": "error", "message": "boom", "error": {"name": "TestError", "message": "it broke"}}"#,
            "\n",
        ),
    )
    .unwrap();
    temp
}

fn parse_stdout(output: &[u8]) -> serde_json::Value {
    let text = String::from_utf8_lossy(output);
    serde_json::from_str(text.trim())
        .unwrap_or_else(|e| panic!("stdout is not a single JSON document: {}\n---\n{}", e, text))
}

fn assert_envelope(value: &serde_json::Value, command: &str) {
    assert_eq!(value["ok"], true, "ok must be true for {}", command);
    assert_eq!(value["command"], command, "command field mismatch");
    assert!(value["version"].is_string(), "{} missing version", command);
    assert!(
        value["generated_at"].is_string(),
        "{} missing generated_at",
        command
    );
    assert!(
        value["elapsed_ms"].is_number(),
        "{} missing elapsed_ms",
        command
    );
    assert!(value["data"].is_object(), "{} missing data object", command);
}

#[test]
fn test_every_command_emits_the_envelope() {
    let temp = setup_project_with_logs();

    let cases: Vec<(&str, Vec<&str>)> = vec![
        ("search", vec!["search", "--level", "error"]),
        ("get", vec!["get", "--request-id", "req-1"]),
        ("stats", vec!["stats", "--by-level"]),
        ("errors", vec!["errors", "--last", "24h"]),
        ("health", vec!["health"]),
        ("schema", vec!["schema"]),
        ("index", vec!["index", "status"]),
        ("index", vec!["index", "rebuild"]),
        ("index", vec!["index", "clean", "--dry-run"]),
        (
            "learn",
            vec!["learn", "--sample", "[2026-03-30] [DB] [ERROR] x"],
        ),
    ];

    for (command, args) in cases {
        let output = somethingsoff(temp.path()).args(&args).output().unwrap();
        let parsed = parse_stdout(&output.stdout);
        assert_envelope(&parsed, command);
    }

    // ingest (needs a file argument)
    let extra = temp.path().join("extra.log");
    fs::write(
        &extra,
        r#"{"timestamp": "2026-03-30T05:02:00.000Z", "level": "info", "message": "extra"}"#,
    )
    .unwrap();
    let output = somethingsoff(temp.path())
        .args(["ingest", "--source", "extra", "--file"])
        .arg(&extra)
        .output()
        .unwrap();
    let parsed = parse_stdout(&output.stdout);
    assert_envelope(&parsed, "ingest");
    assert_eq!(parsed["data"]["entries_indexed"], 1);
}

#[test]
fn test_read_commands_report_sync_in_envelope() {
    let temp = setup_project_with_logs();
    let output = somethingsoff(temp.path())
        .args(["search", "--level", "error"])
        .output()
        .unwrap();
    let parsed = parse_stdout(&output.stdout);
    // First read on a fresh project must have actually synced.
    assert_eq!(parsed["sync"]["skipped"], false);
    assert!(parsed["sync"]["ingested"].as_u64().unwrap() >= 2);

    // Second read: fast path.
    let output = somethingsoff(temp.path())
        .args(["search", "--level", "error"])
        .output()
        .unwrap();
    let parsed = parse_stdout(&output.stdout);
    assert_eq!(parsed["sync"]["skipped"], true);
    assert_eq!(parsed["sync"]["reason"], "fresh");
}

#[test]
fn test_search_meta_shape() {
    let temp = setup_project_with_logs();
    let output = somethingsoff(temp.path())
        .args(["search", "--level", "error", "--limit", "5"])
        .output()
        .unwrap();
    let parsed = parse_stdout(&output.stdout);
    let meta = &parsed["meta"];
    assert_eq!(meta["count"], 1);
    assert_eq!(meta["total"], 1);
    assert_eq!(meta["limit"], 5);
    assert_eq!(meta["offset"], 0);
    assert_eq!(meta["filters"]["level"], "error");
    assert!(meta["time_range"]["start"].is_string());
}

#[test]
fn test_error_envelope_carries_code_hint_exit_code() {
    let temp = setup_project_with_logs();

    // get without a selector → usage error, exit 3
    let output = somethingsoff(temp.path()).arg("get").output().unwrap();
    assert_eq!(output.status.code(), Some(3));
    let parsed = parse_stdout(&output.stdout);
    assert_eq!(parsed["ok"], false);
    assert_eq!(parsed["command"], "get");
    assert_eq!(parsed["error"]["code"], "usage");
    assert_eq!(parsed["error"]["exit_code"], 3);
    assert!(parsed["error"]["hint"]
        .as_str()
        .unwrap()
        .contains("--request-id"));
}

#[test]
fn test_exit_code_table() {
    let temp = setup_project_with_logs();

    // 0: results present
    somethingsoff(temp.path())
        .args(["search", "--level", "error"])
        .assert()
        .code(0);

    // 2: ok but zero results
    somethingsoff(temp.path())
        .args(["search", "--query", "zzz_nonexistent_zzz"])
        .assert()
        .code(2);

    // 3: usage (missing get selector)
    somethingsoff(temp.path()).arg("get").assert().code(3);

    // 3: usage (ingest of missing file)
    somethingsoff(temp.path())
        .args(["ingest", "--source", "x", "--file", "/nonexistent/x.log"])
        .assert()
        .code(3);

    // 3: usage (non-TTY clean without --force)
    // stdin is not a terminal under assert_cmd, so this must refuse.
    somethingsoff(temp.path())
        .args(["index", "clean"])
        .write_stdin("y\n")
        .assert()
        .code(3);

    // 6: parse error (empty learn sample)
    somethingsoff(temp.path())
        .args(["learn", "--sample", "   "])
        .assert()
        .code(6);
}

#[test]
fn test_clean_non_tty_hint_mentions_force() {
    let temp = setup_project_with_logs();
    // Populate the index first so clean has something it *would* delete
    // (old timestamps from 2026-03-30 are beyond the 30-day retention
    // relative to the current test date, making the prompt path reachable).
    somethingsoff(temp.path()).args(["stats"]).assert().code(0);

    let output = somethingsoff(temp.path())
        .args(["index", "clean"])
        .output()
        .unwrap();
    // Either nothing to clean (exit 0) or the non-TTY refusal (exit 3
    // with a --force hint) — both are contract-conforming; what must
    // never happen is a hang or an interactive prompt.
    let parsed = parse_stdout(&output.stdout);
    match output.status.code() {
        Some(0) => assert_eq!(parsed["ok"], true),
        Some(3) => {
            assert_eq!(parsed["error"]["code"], "usage");
            assert!(parsed["error"]["hint"]
                .as_str()
                .unwrap()
                .contains("--force"));
        }
        other => panic!("unexpected exit code {:?}", other),
    }
}

#[test]
fn test_jsonl_strips_envelope() {
    let temp = setup_project_with_logs();
    let output = somethingsoff(temp.path())
        .args(["--format", "jsonl", "search", "--level", "error"])
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&output.stdout);
    let lines: Vec<&str> = text.trim().lines().collect();
    assert_eq!(lines.len(), 1);
    let record: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
    // A bare log entry, not an envelope.
    assert!(record.get("ok").is_none());
    assert_eq!(record["message"], "boom");
}

#[test]
fn test_stdout_is_pure_json_diagnostics_on_stderr() {
    let temp = setup_project_with_logs();
    let output = somethingsoff(temp.path())
        .args(["search", "--level", "error"])
        .output()
        .unwrap();
    // stdout parses as JSON even though the first sync logs progress.
    parse_stdout(&output.stdout);
    // Diagnostics (ingest progress) went to stderr.
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("[INFO]"));

    // --quiet silences stderr entirely.
    let output = somethingsoff(temp.path())
        .args(["--quiet", "stats"])
        .output()
        .unwrap();
    assert!(output.stderr.is_empty());
}

#[test]
fn test_clap_parse_errors_follow_the_error_envelope_contract() {
    // Unknown flags / malformed args are usage errors: exit 3 with a JSON
    // error envelope on stdout — never clap's bare text with exit 2, which
    // would collide with "ok but zero results".
    let temp = TempDir::new().unwrap();
    let output = somethingsoff(temp.path())
        .args(["search", "--bogus-flag"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(3));
    let value = parse_stdout(&output.stdout);
    assert_eq!(value["ok"], false);
    assert_eq!(value["error"]["code"], "usage");
    assert_eq!(value["error"]["exit_code"], 3);
    assert!(value["error"]["hint"].as_str().unwrap().contains("--help"));

    // A value that looks like a flag (real case: cargo's "--> src/..." lines
    // fed to `learn --sample`) — same contract.
    let output = somethingsoff(temp.path())
        .args(["learn", "--sample", "--> src/main.rs:6:6"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(3));
    assert_eq!(parse_stdout(&output.stdout)["ok"], false);

    // --sample=<value> is the documented way to pass leading-dash values.
    let output = somethingsoff(temp.path())
        .args(["learn", "--sample=--> src/main.rs:6:6"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(0));

    // help/version keep their conventional behavior (exit 0, human text).
    let output = somethingsoff(temp.path()).arg("--help").output().unwrap();
    assert_eq!(output.status.code(), Some(0));
}
