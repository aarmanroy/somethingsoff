//! Integration tests for `somethingsoff claude hook` — the Claude Code
//! PostToolUseFailure hook entrypoint.
//!
//! Contract under test: stdin gets a hook event JSON; stdout gets either
//! exactly one hook response JSON line (`hookSpecificOutput.additionalContext`)
//! or nothing; the exit code is always 0. Payload fixtures mirror real
//! events captured from Claude Code (2026-07-17).

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

/// A `PostToolUseFailure` payload as Claude Code actually sends it.
fn failure_payload(cwd: &Path, command: &str) -> String {
    serde_json::json!({
        "hook_event_name": "PostToolUseFailure",
        "tool_name": "Bash",
        "cwd": cwd.to_string_lossy(),
        "tool_input": {"command": command, "description": "test"},
        "error": "Exit code 3\nsome stderr diagnostics",
        "is_interrupt": false,
    })
    .to_string()
}

/// Seed `./logs/` with fresh error entries and build the index by running
/// a read command (the hook itself must never do a first-ever ingest).
fn seed_indexed_errors(project_dir: &Path) {
    let now = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ");
    fs::create_dir(project_dir.join("logs")).unwrap();
    fs::write(
        project_dir.join("logs/app.json"),
        format!(
            "{{\"timestamp\":\"{now}\",\"level\":\"error\",\"message\":\"Payment gateway gw-1 timeout after 3000ms\"}}\n\
             {{\"timestamp\":\"{now}\",\"level\":\"error\",\"message\":\"Payment gateway gw-2 timeout after 4500ms\"}}\n\
             {{\"timestamp\":\"{now}\",\"level\":\"info\",\"message\":\"server started\"}}\n"
        ),
    )
    .unwrap();
    somethingsoff(project_dir)
        .args(["--quiet", "errors", "--last", "1h"])
        .assert()
        .success();
}

fn run_hook(project_dir: &Path, stdin: &str) -> std::process::Output {
    somethingsoff(project_dir)
        .args(["claude", "hook"])
        .write_stdin(stdin.to_string())
        .output()
        .unwrap()
}

fn assert_silent(output: &std::process::Output) {
    assert!(output.status.success(), "hook must always exit 0");
    assert!(
        output.stdout.is_empty(),
        "expected silence, got: {}",
        String::from_utf8_lossy(&output.stdout)
    );
}

fn parse_context(output: &std::process::Output) -> (serde_json::Value, String) {
    assert!(output.status.success(), "hook must always exit 0");
    let raw = String::from_utf8_lossy(&output.stdout);
    let parsed: serde_json::Value = serde_json::from_str(raw.trim())
        .unwrap_or_else(|e| panic!("hook stdout must be one JSON doc ({e}): {raw}"));
    let context = parsed["hookSpecificOutput"]["additionalContext"]
        .as_str()
        .expect("hookSpecificOutput.additionalContext missing")
        .to_string();
    (parsed, context)
}

#[test]
fn test_failure_with_indexed_errors_names_the_top_group() {
    let temp = TempDir::new().unwrap();
    seed_indexed_errors(temp.path());

    let output = run_hook(temp.path(), &failure_payload(temp.path(), "npm test"));
    let (parsed, context) = parse_context(&output);

    assert_eq!(
        parsed["hookSpecificOutput"]["hookEventName"],
        "PostToolUseFailure"
    );
    // The context carries the actual masked template, the exit code, and
    // the exact follow-up command.
    assert!(context.contains("Payment gateway gw-<num> timeout after <num>ms"));
    assert!(context.contains("exit code 3"));
    assert!(context.contains("somethingsoff --quiet errors --last 15m"));
    // Nothing leaks to the envelope: stdout held exactly one JSON doc.
    assert_eq!(parsed.get("ok"), None);
}

#[test]
fn test_repeat_failure_is_rate_limited() {
    let temp = TempDir::new().unwrap();
    seed_indexed_errors(temp.path());
    let payload = failure_payload(temp.path(), "npm test");

    let first = run_hook(temp.path(), &payload);
    parse_context(&first); // fires

    let second = run_hook(temp.path(), &payload);
    assert_silent(&second); // same fingerprint within the window
}

#[test]
fn test_no_index_but_logs_dir_gives_static_hint() {
    let temp = TempDir::new().unwrap();
    fs::create_dir(temp.path().join("logs")).unwrap();
    fs::write(temp.path().join("logs/app.log"), "ERROR boom\n").unwrap();

    let output = run_hook(temp.path(), &failure_payload(temp.path(), "npm test"));
    let (_, context) = parse_context(&output);
    // Static pointer, not a live count — the hook must not trigger the
    // first-ever ingest itself.
    assert!(context.contains("somethingsoff --quiet errors --last 15m"));
    assert!(!context.contains("top group"));
    assert!(
        !temp.path().join(".somethingsoff/index").exists(),
        "hook must not create the index"
    );
}

#[test]
fn test_project_without_logs_or_index_is_silent() {
    let temp = TempDir::new().unwrap();
    let output = run_hook(temp.path(), &failure_payload(temp.path(), "npm test"));
    assert_silent(&output);
}

#[test]
fn test_indexed_but_quiet_window_is_silent() {
    // Errors exist but are older than the 15m lookback: no evidence for
    // *this* failure, so don't spend Claude's tokens.
    let temp = TempDir::new().unwrap();
    fs::create_dir(temp.path().join("logs")).unwrap();
    fs::write(
        temp.path().join("logs/app.json"),
        "{\"timestamp\":\"2020-01-01T00:00:00Z\",\"level\":\"error\",\"message\":\"ancient failure\"}\n",
    )
    .unwrap();
    // Exit 2 = ok but zero results in the window; the index is built.
    somethingsoff(temp.path())
        .args(["--quiet", "errors", "--last", "1h"])
        .assert()
        .code(2);

    let output = run_hook(temp.path(), &failure_payload(temp.path(), "npm test"));
    assert_silent(&output);
}

#[test]
fn test_self_invoking_command_is_silent() {
    let temp = TempDir::new().unwrap();
    seed_indexed_errors(temp.path());
    let output = run_hook(
        temp.path(),
        &failure_payload(temp.path(), "somethingsoff errors --last 1h"),
    );
    assert_silent(&output);
}

#[test]
fn test_non_bash_tool_is_silent() {
    let temp = TempDir::new().unwrap();
    seed_indexed_errors(temp.path());
    let payload = serde_json::json!({
        "hook_event_name": "PostToolUseFailure",
        "tool_name": "Edit",
        "cwd": temp.path().to_string_lossy(),
        "tool_input": {"file_path": "/x"},
        "error": "Exit code 1\n",
    })
    .to_string();
    assert_silent(&run_hook(temp.path(), &payload));
}

#[test]
fn test_successful_command_payload_is_silent() {
    // Real captured PostToolUse success shape: no exit code anywhere.
    let temp = TempDir::new().unwrap();
    seed_indexed_errors(temp.path());
    let payload = serde_json::json!({
        "hook_event_name": "PostToolUse",
        "tool_name": "Bash",
        "cwd": temp.path().to_string_lossy(),
        "tool_input": {"command": "echo hello", "description": "Print hello"},
        "tool_response": {
            "stdout": "hello", "stderr": "", "interrupted": false,
            "isImage": false, "noOutputExpected": false
        },
    })
    .to_string();
    assert_silent(&run_hook(temp.path(), &payload));
}

#[test]
fn test_user_interrupt_is_silent() {
    let temp = TempDir::new().unwrap();
    seed_indexed_errors(temp.path());
    let payload = serde_json::json!({
        "hook_event_name": "PostToolUseFailure",
        "tool_name": "Bash",
        "cwd": temp.path().to_string_lossy(),
        "tool_input": {"command": "sleep 100"},
        "error": "Exit code 130\n",
        "is_interrupt": true,
    })
    .to_string();
    assert_silent(&run_hook(temp.path(), &payload));
}

#[test]
fn test_malformed_and_empty_stdin_are_silent() {
    let temp = TempDir::new().unwrap();
    assert_silent(&run_hook(temp.path(), "not json {{{"));
    assert_silent(&run_hook(temp.path(), ""));
}

// --- Plugin drift guards -------------------------------------------------
// The plugin ships from this repo; these tests keep its metadata and hook
// wiring in lockstep with the binary.

fn plugin_root() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("claude-plugin")
}

#[test]
fn test_plugin_manifest_version_matches_crate() {
    let manifest = plugin_root().join(".claude-plugin/plugin.json");
    let raw = fs::read_to_string(&manifest).unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&raw).unwrap();
    assert_eq!(parsed["name"], env!("CARGO_PKG_NAME"));
    assert_eq!(parsed["version"], env!("CARGO_PKG_VERSION"));
}

#[test]
fn test_plugin_hooks_config_invokes_this_binary() {
    let hooks = plugin_root().join("hooks/hooks.json");
    let raw = fs::read_to_string(&hooks).unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&raw).unwrap();

    let entries = parsed["hooks"]["PostToolUseFailure"].as_array().unwrap();
    assert_eq!(entries[0]["matcher"], "Bash");
    let command = entries[0]["hooks"][0]["command"].as_str().unwrap();
    assert!(command.contains(concat!(env!("CARGO_PKG_NAME"), " claude hook")));
    // Graceful degradation when the binary isn't installed.
    assert!(command.contains("command -v"));
    assert!(command.ends_with("|| true"));
}

#[test]
fn test_plugin_skill_is_the_bundled_skill() {
    // `claude install` embeds this exact file; if it moves, include_str!
    // breaks the build, but keep a fast, explicit guard anyway.
    let skill = plugin_root().join(concat!("skills/", env!("CARGO_PKG_NAME"), "/SKILL.md"));
    let raw = fs::read_to_string(&skill).unwrap();
    assert!(raw.starts_with("---\n"));
    assert!(raw.contains(concat!("name: ", env!("CARGO_PKG_NAME"))));
}
