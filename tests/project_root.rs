//! Project-root resolution: the same question gets the same answer from
//! anywhere inside the project.
//!
//! State and the zero-config `logs/` scan used to be resolved against the
//! process working directory, so `somethingsoff errors` run from `src/` found
//! no sources and answered `ok: true` with zero results — a silent false
//! negative, and indistinguishable from "your logs are clean" to anything
//! reading the JSON contract. It also dropped a second, empty state directory
//! wherever it was run.
//!
//! These tests deliberately do NOT set `SOMETHINGSOFF_BASE_DIR`: an explicit
//! base directory bypasses root resolution, which is the thing under test.

use assert_cmd::Command;
use std::fs;
use std::path::Path;
use tempfile::TempDir;

fn somethingsoff(cwd: &Path) -> Command {
    let mut cmd = Command::cargo_bin("somethingsoff").unwrap();
    cmd.current_dir(cwd);
    cmd
}

/// Asserts on the envelope rather than the exit status: exit 2 means "zero
/// results", which the contract defines as success — the empty-project case
/// below relies on that.
fn response(cwd: &Path, args: &[&str]) -> serde_json::Value {
    let out = somethingsoff(cwd).args(args).assert();
    let stdout = String::from_utf8(out.get_output().stdout.clone()).unwrap();
    let value: serde_json::Value =
        serde_json::from_str(&stdout).unwrap_or_else(|e| panic!("not JSON: {e}\n{stdout}"));
    assert_eq!(value["ok"], true, "command failed: {stdout}");
    value
}

/// A repo with logs at the root and work happening several levels down —
/// the ordinary monorepo shape.
fn repo_with_nested_workdir() -> TempDir {
    let temp = TempDir::new().unwrap();
    // `.git` marks the project boundary.
    fs::create_dir(temp.path().join(".git")).unwrap();
    fs::create_dir(temp.path().join("logs")).unwrap();
    fs::write(
        temp.path().join("logs/app.log"),
        "{\"time\":\"2026-08-10T09:00:00.000Z\",\"level\":\"error\",\"msg\":\"checkout exploded\"}\n",
    )
    .unwrap();
    fs::create_dir_all(temp.path().join("packages/api/src")).unwrap();
    temp
}

#[test]
fn test_query_from_subdirectory_finds_the_projects_logs() {
    let temp = repo_with_nested_workdir();

    let from_root = response(temp.path(), &["--quiet", "errors", "--last", "24h"]);
    assert_eq!(from_root["data"]["total_groups"], 1);

    let nested = temp.path().join("packages/api/src");
    let from_nested = response(&nested, &["--quiet", "errors", "--last", "24h"]);
    assert_eq!(
        from_nested["data"]["total_groups"], 1,
        "a query from a subdirectory must see the same logs as one from the root"
    );
    assert_eq!(
        from_nested["sync"]["files_checked"], 1,
        "the project's log file must be discovered from the subdirectory too"
    );
}

#[test]
fn test_subdirectory_query_does_not_create_a_second_state_dir() {
    let temp = repo_with_nested_workdir();
    let nested = temp.path().join("packages/api/src");
    response(&nested, &["--quiet", "errors", "--last", "24h"]);

    assert!(
        temp.path().join(".somethingsoff").is_dir(),
        "state belongs at the project root"
    );
    assert!(
        !nested.join(".somethingsoff").exists(),
        "running from a subdirectory must not strand a second index there"
    );
}

/// An existing state directory wins over the repo boundary, so a deliberately
/// placed index keeps being used rather than being silently abandoned.
#[test]
fn test_existing_state_dir_takes_precedence_over_repo_root() {
    let temp = repo_with_nested_workdir();
    let package = temp.path().join("packages/api");
    fs::create_dir(package.join(".somethingsoff")).unwrap();
    fs::create_dir(package.join("logs")).unwrap();
    fs::write(
        package.join("logs/api.log"),
        "{\"time\":\"2026-08-10T09:00:00.000Z\",\"level\":\"error\",\"msg\":\"package scoped\"}\n",
    )
    .unwrap();

    let value = response(
        &package.join("src"),
        &["--quiet", "search", "--level", "error", "-n", "10"],
    );
    let messages: Vec<&str> = value["data"]["results"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|e| e["message"].as_str())
        .collect();

    assert_eq!(
        messages,
        vec!["package scoped"],
        "the nearer state directory defines the project"
    );
}

/// Zero results with zero sources is a different answer from zero matches,
/// and the envelope has to carry that difference.
#[test]
fn test_empty_project_says_it_has_no_sources() {
    let temp = TempDir::new().unwrap();
    fs::create_dir(temp.path().join(".git")).unwrap();

    let value = response(temp.path(), &["--quiet", "errors", "--last", "24h"]);
    assert_eq!(value["ok"], true);
    assert_eq!(value["data"]["total_groups"], 0);

    let codes: Vec<&str> = value["notices"]
        .as_array()
        .expect("an empty project must carry notices")
        .iter()
        .filter_map(|n| n["code"].as_str())
        .collect();
    assert!(
        codes.contains(&"no_sources"),
        "an empty result with nothing indexed must say so, got notices: {codes:?}"
    );
}

/// ...and a project that does have sources must not cry wolf.
#[test]
fn test_project_with_sources_emits_no_such_notice() {
    let temp = repo_with_nested_workdir();
    let value = response(temp.path(), &["--quiet", "errors", "--last", "24h"]);
    let codes: Vec<&str> = value["notices"]
        .as_array()
        .map(|n| n.iter().filter_map(|n| n["code"].as_str()).collect())
        .unwrap_or_default();
    assert!(
        !codes.contains(&"no_sources"),
        "sources were found; got notices: {codes:?}"
    );
}
