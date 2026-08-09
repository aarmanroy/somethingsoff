//! Zero-config source discovery: which files get indexed with no setup.
//!
//! Discovery is the difference between the tool working and the tool being
//! silent, so the shapes here are taken from real projects rather than
//! invented. A flat `logs/*.{log,json,jsonl}` scan found three of them.
//!
//! These run through the binary rather than calling `discover_sources`
//! directly, because discovery is relative to the process working directory.

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

/// Write a probe line whose message identifies the file it came from.
fn probe(root: &Path, relative: &str) {
    let path = root.join(relative);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    let marker = relative.replace(['/', '.'], "_");
    fs::write(
        &path,
        format!(
            r#"{{"timestamp":"2026-08-09T10:00:00.000Z","level":"error","message":"probe-{marker}"}}"#
        ) + "\n",
    )
    .unwrap();
}

/// Every message currently reachable in the index.
fn indexed_messages(project: &Path) -> Vec<String> {
    let out = somethingsoff(project)
        .args(["--quiet", "search", "-n", "100", "--compact"])
        .assert()
        .success();
    let stdout = String::from_utf8(out.get_output().stdout.clone()).unwrap();
    let value: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    let mut messages: Vec<String> = value["data"]["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["message"].as_str().unwrap().to_string())
        .collect();
    messages.sort();
    messages
}

#[test]
fn test_discovers_real_world_log_layouts() {
    let temp = TempDir::new().unwrap();
    let root = temp.path();

    let expected = [
        "logs/app.log",         // the original convention
        "logs/app.json",        //
        "logs/app.jsonl",       //
        "logs/app.ndjson",      // newline-delimited JSON
        "logs/app.txt",         // a redirected stream
        "log/development.log",  // Rails, Django, Java
        "logs/sub/nested.log",  // nested one level
        "logs/dep-1/build.log", // nested per-run, e.g. a Nix deploy
        "logs/app.log.1",       // logrotate
        "app.log",              // project root
    ];
    for relative in expected {
        probe(root, relative);
    }

    let messages = indexed_messages(root);
    for relative in expected {
        let marker = format!("probe-{}", relative.replace(['/', '.'], "_"));
        assert!(
            messages.contains(&marker),
            "{relative} was not discovered; indexed: {messages:?}"
        );
    }
}

/// Discovery must not turn the project root into a log directory. Inside
/// `logs/` the directory is evidence enough to trust a `.json` or `.txt`; at
/// the root that same rule would index `package.json`.
#[test]
fn test_root_scan_does_not_swallow_ordinary_files() {
    let temp = TempDir::new().unwrap();
    let root = temp.path();

    probe(root, "logs/app.log"); // so an index exists at all
    fs::write(
        root.join("package.json"),
        r#"{"name":"proj","version":"1"}"#,
    )
    .unwrap();
    fs::write(root.join("tsconfig.json"), r#"{"compilerOptions":{}}"#).unwrap();
    fs::write(root.join("notes.txt"), "just some prose\n").unwrap();
    fs::write(root.join("README.md"), "# hi\n").unwrap();

    let messages = indexed_messages(root);
    assert_eq!(
        messages,
        vec!["probe-logs_app_log".to_string()],
        "only the log should be indexed"
    );
}

/// A compressed rotation has the same filename shape as a live rotation but
/// cannot be read; indexing it would fill the index with binary garbage.
#[test]
fn test_compressed_rotations_are_skipped() {
    let temp = TempDir::new().unwrap();
    let root = temp.path();

    probe(root, "logs/app.log");
    // Not real gzip — the point is that it must never be opened at all.
    fs::write(
        root.join("logs/archive.log.gz"),
        [0x1f, 0x8b, 0x08, 0x00, 0x00],
    )
    .unwrap();
    fs::write(root.join("logs/archive.log.zip"), [0x50, 0x4b, 0x03, 0x04]).unwrap();

    let messages = indexed_messages(root);
    assert_eq!(messages, vec!["probe-logs_app_log".to_string()]);
}

/// Two runs can each hold an `app.log`. Naming both `app` would make
/// `--source app` mean two different things.
#[test]
fn test_nested_sources_are_named_by_path() {
    let temp = TempDir::new().unwrap();
    let root = temp.path();

    probe(root, "logs/dep-1/app.log");
    probe(root, "logs/dep-2/app.log");

    let out = somethingsoff(root)
        .args(["--quiet", "search", "-n", "10", "--compact"])
        .assert()
        .success();
    let stdout = String::from_utf8(out.get_output().stdout.clone()).unwrap();
    let value: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    let mut sources: Vec<&str> = value["data"]["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["source"].as_str().unwrap())
        .collect();
    sources.sort();
    sources.dedup();
    assert_eq!(sources, vec!["dep-1/app", "dep-2/app"]);
}

/// A rotated file belongs to the same logical source as the live one, so
/// `--source app` covers both rather than silently omitting yesterday.
#[test]
fn test_rotated_file_shares_the_live_source_name() {
    let temp = TempDir::new().unwrap();
    let root = temp.path();

    probe(root, "logs/app.log");
    probe(root, "logs/app.log.1");

    let out = somethingsoff(root)
        .args([
            "--quiet",
            "search",
            "--source",
            "app",
            "-n",
            "10",
            "--compact",
        ])
        .assert()
        .success();
    let stdout = String::from_utf8(out.get_output().stdout.clone()).unwrap();
    let value: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(
        value["meta"]["total"], 2,
        "--source app must cover the rotated file too"
    );
}
