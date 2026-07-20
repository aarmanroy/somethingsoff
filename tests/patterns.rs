//! Integration tests for the `patterns` command (Drain template mining).

use assert_cmd::Command;
use chrono::{Duration, Utc};
use std::fs;
use std::path::Path;
use tempfile::TempDir;

fn somethingsoff(project_dir: &Path) -> Command {
    let mut cmd = Command::cargo_bin("somethingsoff").unwrap();
    cmd.current_dir(project_dir)
        .env("SOMETHINGSOFF_BASE_DIR", project_dir.join(".somethingsoff"));
    cmd
}

fn parse_stdout(output: &[u8]) -> serde_json::Value {
    let text = String::from_utf8_lossy(output);
    serde_json::from_str(text.trim())
        .unwrap_or_else(|e| panic!("stdout is not a single JSON document: {}\n---\n{}", e, text))
}

/// Recent, strictly increasing ISO timestamp `n` steps into the corpus.
fn ts(n: i64) -> String {
    (Utc::now() - Duration::minutes(10) + Duration::seconds(n))
        .format("%Y-%m-%dT%H:%M:%S%.3fZ")
        .to_string()
}

/// Three obvious message families: 8 GET requests, 5 DB timeouts, 2 one-offs.
fn setup_project() -> TempDir {
    let temp = TempDir::new().unwrap();
    let logs_dir = temp.path().join("logs");
    fs::create_dir(&logs_dir).unwrap();

    let mut lines = String::new();
    let mut n = 0;
    for i in 1..=8 {
        lines.push_str(&format!(
            "{{\"timestamp\": \"{}\", \"level\": \"info\", \"message\": \"GET /api/users/{} 200 in {}ms\"}}\n",
            ts(n), i, i * 10,
        ));
        n += 1;
    }
    for i in 1..=5 {
        lines.push_str(&format!(
            "{{\"timestamp\": \"{}\", \"level\": \"error\", \"message\": \"Connection timeout to db-{} after {}ms\"}}\n",
            ts(n), i, i * 100,
        ));
        n += 1;
    }
    lines.push_str(&format!(
        "{{\"timestamp\": \"{}\", \"level\": \"info\", \"message\": \"service booted successfully\"}}\n",
        ts(n),
    ));
    lines.push_str(&format!(
        "{{\"timestamp\": \"{}\", \"level\": \"warn\", \"message\": \"config key deprecated_flag ignored entirely\"}}\n",
        ts(n + 1),
    ));
    fs::write(logs_dir.join("app.log"), lines).unwrap();
    temp
}

#[test]
fn test_patterns_groups_families_and_emits_envelope() {
    let temp = setup_project();
    let output = somethingsoff(temp.path())
        .args(["patterns", "--last", "1h"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(0));
    let parsed = parse_stdout(&output.stdout);

    assert_eq!(parsed["ok"], true);
    assert_eq!(parsed["command"], "patterns");
    assert!(parsed["version"].is_string());
    assert!(parsed["elapsed_ms"].is_number());

    let data = &parsed["data"];
    assert_eq!(data["total_logs"], 15);
    assert_eq!(data["scanned"], 15);

    let templates = data["templates"].as_array().unwrap();
    assert_eq!(parsed["meta"]["count"], templates.len() as i64);
    assert_eq!(parsed["meta"]["total"], 15);
    assert_eq!(parsed["meta"]["filters"]["last"], "1h");

    // Top template is the GET family, fully generalized by the masks.
    let top = &templates[0];
    assert_eq!(top["count"], 8);
    assert_eq!(top["template"], "GET <path> <num> in <dur>");
    assert!(top["template_id"].as_str().unwrap().starts_with("v1:"));
    assert_eq!(top["levels"]["info"], 8);
    assert!(top["sample_message"]
        .as_str()
        .unwrap()
        .starts_with("GET /api/users/"));
    assert_eq!(top["sample_log_ids"].as_array().unwrap().len(), 3);
    assert!(top["share_pct"].as_f64().unwrap() > 50.0);
    assert!(top["first_seen"].as_str().unwrap() <= top["last_seen"].as_str().unwrap());

    // Second template is the timeout family.
    let second = &templates[1];
    assert_eq!(second["count"], 5);
    assert_eq!(
        second["template"],
        "Connection timeout to db-<num> after <dur>"
    );
    assert_eq!(second["levels"]["error"], 5);
}

#[test]
fn test_patterns_level_filter() {
    let temp = setup_project();
    let output = somethingsoff(temp.path())
        .args(["patterns", "--last", "1h", "--level", "error"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(0));
    let parsed = parse_stdout(&output.stdout);
    assert_eq!(parsed["data"]["total_logs"], 5);
    let templates = parsed["data"]["templates"].as_array().unwrap();
    assert_eq!(templates.len(), 1);
    assert_eq!(
        templates[0]["template"],
        "Connection timeout to db-<num> after <dur>"
    );
}

#[test]
fn test_patterns_source_filter_and_exit_2() {
    let temp = setup_project();
    // A second source with its own repeated message.
    let worker_lines: String = (0..4)
        .map(|i| {
            format!(
                "{{\"timestamp\": \"{}\", \"level\": \"info\", \"message\": \"job {} finished cleanly\"}}\n",
                ts(60 + i), i,
            )
        })
        .collect();
    fs::write(temp.path().join("logs/worker.log"), worker_lines).unwrap();

    let output = somethingsoff(temp.path())
        .args(["patterns", "--last", "1h", "--source", "worker"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(0));
    let parsed = parse_stdout(&output.stdout);
    assert_eq!(parsed["data"]["total_logs"], 4);
    let templates = parsed["data"]["templates"].as_array().unwrap();
    assert_eq!(templates.len(), 1);
    // The regex masks catch the digit before Drain does, so the slot is
    // the more informative <num>, not the generic <*>.
    assert_eq!(templates[0]["template"], "job <num> finished cleanly");
    assert_eq!(parsed["meta"]["filters"]["source"], "worker");

    // Zero matches → ok envelope, exit 2.
    let output = somethingsoff(temp.path())
        .args(["patterns", "--last", "1h", "--source", "nosuchsource"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
    let parsed = parse_stdout(&output.stdout);
    assert_eq!(parsed["ok"], true);
    assert_eq!(parsed["data"]["total_logs"], 0);
    assert_eq!(parsed["data"]["templates"].as_array().unwrap().len(), 0);
}

#[test]
fn test_patterns_jsonl_strips_envelope() {
    let temp = setup_project();
    let output = somethingsoff(temp.path())
        .args(["--format", "jsonl", "patterns", "--last", "1h"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(0));
    let text = String::from_utf8_lossy(&output.stdout);
    let lines: Vec<&str> = text.trim().lines().collect();
    assert!(lines.len() >= 2);
    for line in lines {
        let record: serde_json::Value = serde_json::from_str(line).unwrap();
        assert!(record.get("ok").is_none(), "jsonl must strip the envelope");
        assert!(record["template"].is_string());
        assert!(record["count"].is_number());
    }
}

#[test]
fn test_patterns_limit_truncates() {
    let temp = setup_project();
    let output = somethingsoff(temp.path())
        .args(["patterns", "--last", "1h", "-n", "1"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(0));
    let parsed = parse_stdout(&output.stdout);
    let templates = parsed["data"]["templates"].as_array().unwrap();
    assert_eq!(templates.len(), 1);
    assert_eq!(templates[0]["count"], 8, "highest-count template survives");
    assert_eq!(parsed["meta"]["limit"], 1);
}
