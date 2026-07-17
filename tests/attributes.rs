//! Integration tests for schema flexibility: alias mapping, attribute
//! capture, searchability, PII redaction in attributes, and the
//! discover-style schema profile.

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

fn setup(content: &str) -> TempDir {
    let temp = TempDir::new().unwrap();
    fs::create_dir(temp.path().join("logs")).unwrap();
    fs::write(temp.path().join("logs/app.log"), content).unwrap();
    temp
}

fn search_json(temp: &TempDir, args: &[&str]) -> serde_json::Value {
    let output = somethingsoff(temp.path()).args(args).output().unwrap();
    serde_json::from_str(String::from_utf8_lossy(&output.stdout).trim()).unwrap()
}

#[test]
fn test_camelcase_requestid_maps_to_request_id() {
    let temp = setup(
        r#"{"timestamp": "2026-07-15T10:00:00.000Z", "level": "error", "msg": "payment failed", "requestId": "req-42", "userId": "u-7"}
"#,
    );

    // camelCase requestId is queryable through the canonical filter.
    let parsed = search_json(&temp, &["search", "--request-id", "req-42"]);
    assert_eq!(parsed["meta"]["total"], 1);
    let entry = &parsed["data"]["results"][0];
    assert_eq!(entry["request_id"], "req-42");
    assert_eq!(entry["user_id"], "u-7");
    // `msg` alias mapped to message
    assert_eq!(entry["message"], "payment failed");
}

#[test]
fn test_unknown_fields_preserved_in_attributes_and_searchable() {
    let temp = setup(
        r#"{"timestamp": "2026-07-15T10:00:00.000Z", "level": "info", "message": "charge ok", "service": "checkout", "paymentIntent": "pi_9X7abc", "region": "eu-west-1"}
"#,
    );

    // Preserved and returned:
    let parsed = search_json(&temp, &["search", "--level", "info"]);
    let attrs = &parsed["data"]["results"][0]["attributes"];
    assert_eq!(attrs["service"], "checkout");
    assert_eq!(attrs["paymentIntent"], "pi_9X7abc");
    assert_eq!(attrs["region"], "eu-west-1");

    // Attribute values reachable via full-text query:
    let parsed = search_json(&temp, &["search", "--query", "pi_9X7abc"]);
    assert_eq!(parsed["meta"]["total"], 1);
}

#[test]
fn test_pii_redacted_inside_attribute_values() {
    let temp = setup(
        r#"{"timestamp": "2026-07-15T10:00:00.000Z", "level": "info", "message": "login", "contact": "reach me at alice@example.com"}
"#,
    );

    let parsed = search_json(&temp, &["search", "--level", "info"]);
    let contact = parsed["data"]["results"][0]["attributes"]["contact"]
        .as_str()
        .unwrap();
    assert!(
        !contact.contains("alice@example.com"),
        "email must be redacted in attributes, got: {}",
        contact
    );
    assert!(contact.contains("REDACTED"));
}

#[test]
fn test_status_and_slow_filters() {
    let temp = setup(concat!(
        r#"{"timestamp": "2026-07-15T10:00:00.000Z", "level": "info", "message": "ok fast", "statusCode": 200, "durationMs": 12}"#,
        "\n",
        r#"{"timestamp": "2026-07-15T10:00:01.000Z", "level": "error", "message": "server broke", "statusCode": 503, "durationMs": 4300}"#,
        "\n",
    ));

    // Range filter on aliased camelCase statusCode:
    let parsed = search_json(&temp, &["search", "--status", "500-599"]);
    assert_eq!(parsed["meta"]["total"], 1);
    assert_eq!(parsed["data"]["results"][0]["status_code"], 503);

    // Slow filter on aliased durationMs:
    let parsed = search_json(&temp, &["search", "--slow-above", "1000"]);
    assert_eq!(parsed["meta"]["total"], 1);
    assert_eq!(parsed["data"]["results"][0]["message"], "server broke");
}

#[test]
fn test_logfmt_lines_are_parsed() {
    let temp = setup(concat!(
        r#"time=2026-07-15T10:00:00Z level=error msg="db connection refused" service=checkout"#,
        "\n",
    ));

    let parsed = search_json(&temp, &["search", "--level", "error"]);
    assert_eq!(parsed["meta"]["total"], 1);
    let entry = &parsed["data"]["results"][0];
    assert_eq!(entry["message"], "db connection refused");
    assert_eq!(entry["attributes"]["service"], "checkout");
}

#[test]
fn test_errors_template_groups_near_duplicates() {
    // Timestamps relative to now so the --last 24h window can't rot.
    let base = chrono::Utc::now() - chrono::Duration::minutes(5);
    let mut lines = String::new();
    for i in 0..5 {
        let ts = (base + chrono::Duration::seconds(i)).format("%Y-%m-%dT%H:%M:%S%.3fZ");
        lines.push_str(&format!(
            r#"{{"timestamp": "{ts}", "level": "error", "message": "x", "error": {{"name": "TimeoutError", "message": "Connection timeout to db-{i}.internal after {i}00ms"}}}}"#,
        ));
        lines.push('\n');
    }
    let temp = setup(&lines);

    let output = somethingsoff(temp.path())
        .args(["errors", "--last", "24h"])
        .output()
        .unwrap();
    let parsed: serde_json::Value =
        serde_json::from_str(String::from_utf8_lossy(&output.stdout).trim()).unwrap();

    // All 5 near-duplicates collapse into ONE group with a masked template.
    assert_eq!(parsed["data"]["total_errors"], 5);
    assert_eq!(parsed["data"]["total_groups"], 1);
    assert_eq!(
        parsed["data"]["groups"][0]["template"],
        "Connection timeout to db-<num>.internal after <num>ms"
    );
    assert_eq!(parsed["data"]["groups"][0]["count"], 5);
}

#[test]
fn test_schema_profiles_actual_fields() {
    let temp = setup(
        r#"{"timestamp": "2026-07-15T10:00:00.000Z", "level": "info", "message": "hello", "service": "checkout"}
"#,
    );

    let output = somethingsoff(temp.path()).arg("schema").output().unwrap();
    let parsed: serde_json::Value =
        serde_json::from_str(String::from_utf8_lossy(&output.stdout).trim()).unwrap();

    let fields = parsed["data"]["schema"]["fields"].as_array().unwrap();
    let names: Vec<&str> = fields.iter().map(|f| f["name"].as_str().unwrap()).collect();
    assert!(names.contains(&"level"));
    assert!(names.contains(&"attributes.service"));
    // Fields absent from the data are not fabricated:
    assert!(!names.contains(&"user_id"));

    let service = fields
        .iter()
        .find(|f| f["name"] == "attributes.service")
        .unwrap();
    assert_eq!(service["count"], 1);
    assert_eq!(service["samples"][0], "checkout");
}

#[test]
fn test_schema_version_migration_is_transparent() {
    let temp = setup(
        r#"{"timestamp": "2026-07-15T10:00:00.000Z", "level": "error", "message": "survives migration"}
"#,
    );

    // Build the index, then simulate an old index by rewriting the version.
    somethingsoff(temp.path())
        .args(["stats"])
        .assert()
        .success();
    let version_file = temp.path().join(".somethingsoff/index/schema_version");
    assert!(version_file.exists(), "version file should be stamped");
    fs::write(&version_file, "1").unwrap();

    // Next read migrates transparently and still answers correctly.
    let parsed = search_json(&temp, &["search", "--level", "error"]);
    assert_eq!(parsed["ok"], true);
    assert_eq!(parsed["meta"]["total"], 1);
    assert_eq!(parsed["sync"]["migrated"], true);

    // Version file restored to current.
    let restored = fs::read_to_string(&version_file).unwrap();
    assert_ne!(restored.trim(), "1");
}
