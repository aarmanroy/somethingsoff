//! Integration tests for `somethingsoff claude hook` — the Claude Code
//! PostToolUseFailure and PreToolUse hook entrypoint.
//!
//! Contract under test: stdin gets a hook event JSON; stdout gets either
//! exactly one hook response JSON line (`hookSpecificOutput`) or nothing;
//! the exit code is always 0. Payload fixtures mirror real events captured
//! from Claude Code (2026-07-17).

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

/// Build the index by running a read command — the hook itself must never
/// do a first-ever ingest.
fn build_index(project_dir: &Path) {
    somethingsoff(project_dir)
        .args(["--quiet", "errors", "--last", "1h"])
        .assert()
        .success();
}

/// Seed `./logs/app.json` with fresh errors, comfortably past
/// `MIN_RAW_BYTES` so the deny is worth making. See
/// `test_a_small_log_is_cheaper_to_read_whole` for the other side.
fn seed_indexed_errors(project_dir: &Path) {
    let now = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ");
    fs::create_dir(project_dir.join("logs")).unwrap();
    let mut log = String::new();
    for i in 0..60 {
        log.push_str(&format!(
            "{{\"timestamp\":\"{now}\",\"level\":\"error\",\"message\":\"Payment gateway gw-{i} timeout after {i}000ms\"}}\n"
        ));
    }
    log.push_str(&format!(
        "{{\"timestamp\":\"{now}\",\"level\":\"info\",\"message\":\"server started\"}}\n"
    ));
    fs::write(project_dir.join("logs/app.json"), log).unwrap();
    build_index(project_dir);
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

// --- PreToolUse: answer instead of letting the raw file through ----------

/// A `PreToolUse` payload as Claude Code sends it for a Bash call.
fn pre_payload(cwd: &Path, command: &str) -> String {
    serde_json::json!({
        "hook_event_name": "PreToolUse",
        "tool_name": "Bash",
        "cwd": cwd.to_string_lossy(),
        "tool_input": {"command": command, "description": "test"},
    })
    .to_string()
}

fn parse_deny(output: &std::process::Output) -> String {
    assert!(output.status.success(), "hook must always exit 0");
    let raw = String::from_utf8_lossy(&output.stdout);
    let parsed: serde_json::Value = serde_json::from_str(raw.trim())
        .unwrap_or_else(|e| panic!("hook stdout must be one JSON doc ({e}): {raw}"));
    assert_eq!(parsed["hookSpecificOutput"]["hookEventName"], "PreToolUse");
    assert_eq!(parsed["hookSpecificOutput"]["permissionDecision"], "deny");
    parsed["hookSpecificOutput"]["permissionDecisionReason"]
        .as_str()
        .expect("permissionDecisionReason missing")
        .to_string()
}

#[test]
fn test_reading_an_indexed_log_is_denied_with_the_answer() {
    let temp = TempDir::new().unwrap();
    seed_indexed_errors(temp.path());

    let output = run_hook(temp.path(), &pre_payload(temp.path(), "cat logs/app.json"));
    let reason = parse_deny(&output);

    // The reason *is* the answer: the query already ran.
    assert!(reason.contains("Payment gateway gw-<num> timeout after <num>ms"));
    assert!(reason.contains("logs/app.json"));
    // Both follow-ups, and the escape hatch.
    assert!(reason.contains("somethingsoff --quiet errors --last 1h"));
    // The raw-lines follow-up is scoped to the source that was read.
    assert!(reason.contains("somethingsoff --quiet search --last 1h --source app -n 50 --compact"));
    assert!(reason.contains("Re-run this exact command"));
}

#[test]
fn test_repeating_a_denied_command_is_allowed_through() {
    // The escape hatch must be real: a hook may never trap the agent.
    let temp = TempDir::new().unwrap();
    seed_indexed_errors(temp.path());
    let payload = pre_payload(temp.path(), "cat logs/app.json");

    parse_deny(&run_hook(temp.path(), &payload)); // denied once
    assert_silent(&run_hook(temp.path(), &payload)); // retry runs
}

#[test]
fn test_reading_a_non_log_file_is_allowed() {
    let temp = TempDir::new().unwrap();
    seed_indexed_errors(temp.path());
    for command in ["cat README.md", "cat package.json", "npm test"] {
        assert_silent(&run_hook(temp.path(), &pre_payload(temp.path(), command)));
    }
}

#[test]
fn test_pre_tool_use_without_an_index_is_allowed() {
    // No index: nothing to offer, and a hook must not build one.
    let temp = TempDir::new().unwrap();
    fs::create_dir(temp.path().join("logs")).unwrap();
    fs::write(temp.path().join("logs/app.log"), "ERROR boom\n").unwrap();

    let output = run_hook(temp.path(), &pre_payload(temp.path(), "cat logs/app.log"));
    assert_silent(&output);
    assert!(
        !temp.path().join(".somethingsoff/index").exists(),
        "hook must not create the index"
    );
}

#[test]
fn test_pre_tool_use_with_a_quiet_window_is_allowed() {
    // Indexed, but no errors in the window: the raw file is still the
    // better answer, so don't block it.
    let temp = TempDir::new().unwrap();
    fs::create_dir(temp.path().join("logs")).unwrap();
    fs::write(
        temp.path().join("logs/app.json"),
        "{\"timestamp\":\"2020-01-01T00:00:00Z\",\"level\":\"error\",\"message\":\"ancient failure\"}\n",
    )
    .unwrap();
    somethingsoff(temp.path())
        .args(["--quiet", "errors", "--last", "1h"])
        .assert()
        .code(2);

    assert_silent(&run_hook(
        temp.path(),
        &pre_payload(temp.path(), "cat logs/app.json"),
    ));
}

/// Seed two sources: a loud one, and a quieter one whose own top error is
/// completely different. Both past `MIN_RAW_BYTES`.
fn seed_two_sources(project_dir: &Path) {
    let now = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ");
    fs::create_dir(project_dir.join("logs")).unwrap();
    let mut payments = String::new();
    for i in 0..60 {
        payments.push_str(&format!(
            "{{\"timestamp\":\"{now}\",\"level\":\"error\",\"message\":\"Payment gateway gw-{i} timeout\"}}\n"
        ));
    }
    fs::write(project_dir.join("logs/payments.json"), payments).unwrap();

    let mut auth = String::new();
    for i in 0..12 {
        auth.push_str(&format!(
            "{{\"timestamp\":\"{now}\",\"level\":\"error\",\"message\":\"JWT signature verification failed for tenant acme-{i}\"}}\n"
        ));
    }
    for i in 0..30 {
        auth.push_str(&format!(
            "{{\"timestamp\":\"{now}\",\"level\":\"info\",\"message\":\"issued session token for tenant acme-{i}\"}}\n"
        ));
    }
    fs::write(project_dir.join("logs/auth.json"), auth).unwrap();
    build_index(project_dir);
}

/// Regression: the deny used to report the whole index's top group, so
/// asking about `auth.json` while debugging a JWT bug answered with
/// payment-gateway timeouts — and blocked the file holding the answer.
#[test]
fn test_deny_answers_about_the_file_that_was_asked_for() {
    let temp = TempDir::new().unwrap();
    seed_two_sources(temp.path());

    let output = run_hook(temp.path(), &pre_payload(temp.path(), "cat logs/auth.json"));
    let reason = parse_deny(&output);

    assert!(
        reason.contains("JWT signature verification failed"),
        "must answer about auth.json, got: {reason}"
    );
    assert!(
        !reason.contains("Payment gateway"),
        "must not answer with a louder neighbour's errors: {reason}"
    );
    // The follow-up stays on the same slice the summary described.
    assert!(reason.contains("--source auth"));
}

/// A directory read is genuinely about everything, so it answers across
/// the index — and says so.
#[test]
fn test_directory_read_answers_across_the_index() {
    let temp = TempDir::new().unwrap();
    seed_two_sources(temp.path());

    let reason = parse_deny(&run_hook(
        temp.path(),
        &pre_payload(temp.path(), "grep -r timeout logs/"),
    ));
    assert!(reason.contains("Payment gateway")); // the actual top group
    assert!(!reason.contains("--source"));
}

/// Regression: any `.log`-shaped path was denied and answered with this
/// project's errors, even for files the index has never seen.
#[test]
fn test_log_shaped_paths_outside_the_index_run_untouched() {
    let temp = TempDir::new().unwrap();
    seed_indexed_errors(temp.path());
    for command in [
        "cat /var/log/system.log",
        "cat npm-debug.log",
        "tail -100 /tmp/build.log",
        "cat node_modules/.cache/debug.log",
    ] {
        assert_silent(&run_hook(temp.path(), &pre_payload(temp.path(), command)));
    }
}

/// Regression: for `grep`-likes the first non-flag word is the pattern.
/// Searching source code *for* a log path is not a log read.
#[test]
fn test_searching_source_code_for_a_log_path_is_allowed() {
    let temp = TempDir::new().unwrap();
    seed_indexed_errors(temp.path());
    for command in [
        "grep -rn 'logs/app.json' src/",
        "rg logs/app.json --files-with-matches",
    ] {
        assert_silent(&run_hook(temp.path(), &pre_payload(temp.path(), command)));
    }
}

/// Regression: with a single remembered fingerprint, an agent that
/// alternated two phrasings clobbered its own escape hatch and was denied
/// forever. The escape hatch must survive interleaving.
#[test]
fn test_alternating_reads_never_trap_the_agent() {
    let temp = TempDir::new().unwrap();
    seed_indexed_errors(temp.path());
    let cat = pre_payload(temp.path(), "cat logs/app.json");
    let tail = pre_payload(temp.path(), "tail -50 logs/app.json");

    parse_deny(&run_hook(temp.path(), &cat)); // denied once, answer given
    parse_deny(&run_hook(temp.path(), &tail)); // a different read, also once
    assert_silent(&run_hook(temp.path(), &cat)); // the retry gets through
    assert_silent(&run_hook(temp.path(), &tail)); // and so does this one
}

/// Regression: the answer is only a saving if it is smaller than what it
/// replaces. On a short log the summary cost *more* context than the file
/// and told Claude less — so a small file is simply read.
#[test]
fn test_a_small_log_is_cheaper_to_read_whole() {
    let temp = TempDir::new().unwrap();
    let now = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ");
    fs::create_dir(temp.path().join("logs")).unwrap();
    fs::write(
        temp.path().join("logs/app.json"),
        format!(
            "{{\"timestamp\":\"{now}\",\"level\":\"error\",\"message\":\"boom in the widget factory\"}}\n"
        ),
    )
    .unwrap();
    build_index(temp.path());

    // Fresh errors are indexed and would produce a summary — but the file
    // is 90 bytes, so blocking it would spend tokens to lose information.
    assert!(temp.path().join("logs/app.json").metadata().unwrap().len() < 4096);
    assert_silent(&run_hook(
        temp.path(),
        &pre_payload(temp.path(), "cat logs/app.json"),
    ));
}

/// …but a follow never ends, however small the file is right now.
#[test]
fn test_following_a_small_log_is_still_intercepted() {
    let temp = TempDir::new().unwrap();
    let now = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ");
    fs::create_dir(temp.path().join("logs")).unwrap();
    fs::write(
        temp.path().join("logs/app.json"),
        format!(
            "{{\"timestamp\":\"{now}\",\"level\":\"error\",\"message\":\"boom in the widget factory\"}}\n"
        ),
    )
    .unwrap();
    build_index(temp.path());

    let reason = parse_deny(&run_hook(
        temp.path(),
        &pre_payload(temp.path(), "tail -f logs/app.json"),
    ));
    assert!(reason.contains("boom in the widget factory"));
}

// --- PreToolUse on Claude's own Read tool --------------------------------

fn read_payload(cwd: &Path, file_path: &str) -> String {
    serde_json::json!({
        "hook_event_name": "PreToolUse",
        "tool_name": "Read",
        "cwd": cwd.to_string_lossy(),
        "tool_input": {"file_path": file_path},
    })
    .to_string()
}

/// Shelling out to `cat` is not how an agent usually reads a file — the
/// `Read` tool is. It must be covered by the same answer.
#[test]
fn test_read_tool_on_an_indexed_log_is_denied_with_the_answer() {
    let temp = TempDir::new().unwrap();
    seed_indexed_errors(temp.path());

    let abs = temp.path().join("logs/app.json");
    let reason = parse_deny(&run_hook(
        temp.path(),
        &read_payload(temp.path(), &abs.to_string_lossy()),
    ));
    assert!(reason.contains("Payment gateway gw-<num> timeout after <num>ms"));
    assert!(reason.contains("--source app"));
}

#[test]
fn test_read_tool_on_ordinary_files_is_allowed() {
    let temp = TempDir::new().unwrap();
    seed_indexed_errors(temp.path());
    fs::write(temp.path().join("README.md"), "# hi\n").unwrap();
    let readme = temp.path().join("README.md");
    assert_silent(&run_hook(
        temp.path(),
        &read_payload(temp.path(), &readme.to_string_lossy()),
    ));
}

#[test]
fn test_read_tool_is_not_treated_as_a_command_failure() {
    // The failure hook stays Bash-only; a Read event must never reach it.
    let temp = TempDir::new().unwrap();
    seed_indexed_errors(temp.path());
    let payload = serde_json::json!({
        "hook_event_name": "PostToolUseFailure",
        "tool_name": "Read",
        "cwd": temp.path().to_string_lossy(),
        "tool_input": {"file_path": "/x"},
        "error": "Exit code 1\n",
    })
    .to_string();
    assert_silent(&run_hook(temp.path(), &payload));
}

#[test]
fn test_pre_tool_use_never_intercepts_our_own_commands() {
    let temp = TempDir::new().unwrap();
    seed_indexed_errors(temp.path());
    assert_silent(&run_hook(
        temp.path(),
        &pre_payload(temp.path(), "somethingsoff --quiet search --last 1h"),
    ));
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

    // Both events route to the same entrypoint; it dispatches on
    // hook_event_name. `PreToolUse` also covers Claude's own `Read` tool,
    // which is how an agent reads a log file when it doesn't shell out.
    for (event, matcher) in [("PreToolUse", "Bash|Read"), ("PostToolUseFailure", "Bash")] {
        let entries = parsed["hooks"][event]
            .as_array()
            .unwrap_or_else(|| panic!("{event} not registered"));
        assert_eq!(entries[0]["matcher"], matcher);
        let command = entries[0]["hooks"][0]["command"].as_str().unwrap();
        assert!(command.contains(concat!(env!("CARGO_PKG_NAME"), " claude hook")));
        // Graceful degradation when the binary isn't installed.
        assert!(command.contains("command -v"));
        assert!(command.ends_with("|| true"));
    }
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
