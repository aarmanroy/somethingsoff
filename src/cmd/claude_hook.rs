//! Claude Code hook entrypoint: `somethingsoff claude hook`.
//!
//! The bundled plugin (`claude-plugin/hooks/hooks.json`) runs this on
//! `PostToolUseFailure` for Bash tool calls. Stdin is Claude Code's hook
//! event JSON; stdout is an optional hook response whose
//! `additionalContext` points Claude at the log evidence for the failure —
//! ideally the actual top error group from the freshly synced index.
//!
//! This command is exempt from the v1 output envelope (see
//! docs/CONTRACT.md): its stdout belongs to Claude Code's hook protocol.
//! A hook must never become noise, so every path that can't produce a
//! useful suggestion exits 0 silently — malformed payloads, missing state,
//! query errors, and timeouts included.

use std::io::Read;
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use sha2::Digest;

use crate::config::Config;
use crate::index::searcher::SearchOptions;

/// Failing commands can embed sizable build output in the payload's
/// `error` field; accept real-world payloads but stay bounded.
const MAX_STDIN_BYTES: u64 = 8 * 1024 * 1024;
/// Time window the hook queries — and tells Claude to query.
const LOOKBACK: &str = "15m";
/// Error groups fetched for the summary (only the top one is named).
const TOP_GROUPS: usize = 3;
/// Same-fingerprint suggestions are suppressed for this long.
const RATE_LIMIT: Duration = Duration::from_secs(300);
/// Budget for auto-sync + query; on overrun fall back to the static hint.
const QUERY_BUDGET: Duration = Duration::from_secs(3);
/// Ceiling for the injected context — hooks should be cheap in tokens.
const MAX_CONTEXT_CHARS: usize = 600;
/// Longest masked template quoted inside the context.
const MAX_TEMPLATE_CHARS: usize = 160;

/// The subset of Claude Code's hook event payload the hook cares about.
/// Every field is optional-with-default: an unexpected shape must degrade
/// to silence, not to a parse error.
#[derive(Deserialize, Default)]
#[serde(default)]
struct HookPayload {
    hook_event_name: String,
    tool_name: String,
    cwd: String,
    tool_input: ToolInput,
    /// `PostToolUseFailure` carries `"Exit code N\n<output>"` here.
    error: Option<String>,
    is_interrupt: bool,
    /// `PostToolUse` result object, probed for explicit failure signals.
    tool_response: serde_json::Value,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct ToolInput {
    command: String,
}

/// Rate-limit state, kept in the OS temp dir (keyed by project cwd) so the
/// hook never creates `./.somethingsoff/` in a project on its own.
#[derive(Deserialize, Serialize, Default)]
#[serde(default)]
struct HookState {
    last_fired_unix: u64,
    last_fingerprint: String,
}

/// Entry point. Infallible by design: always exits 0, prints either one
/// hook-response JSON line or nothing.
pub async fn run() -> u8 {
    crate::set_quiet(true);

    let Some(payload) = read_payload() else {
        return 0;
    };
    let Some(exit_code) = detect_failure(&payload) else {
        return 0;
    };
    // Never nag about our own invocations (including the skill's recipes).
    if payload.tool_input.command.contains(env!("CARGO_PKG_NAME")) {
        return 0;
    }
    // All project state (config, index, logs dir) is cwd-relative.
    if payload.cwd.is_empty() || std::env::set_current_dir(&payload.cwd).is_err() {
        return 0;
    }

    let Some((context, fingerprint)) = build_context(exit_code).await else {
        return 0;
    };
    if rate_limited(&payload.cwd, &fingerprint) {
        return 0;
    }

    let response = serde_json::json!({
        "hookSpecificOutput": {
            "hookEventName": payload.hook_event_name,
            "additionalContext": context,
        }
    });
    println!("{}", response);
    0
}

fn read_payload() -> Option<HookPayload> {
    let mut raw = String::new();
    std::io::stdin()
        .lock()
        .take(MAX_STDIN_BYTES)
        .read_to_string(&mut raw)
        .ok()?;
    serde_json::from_str(&raw).ok()
}

/// Did this Bash tool call fail? Returns the exit code when known.
///
/// Reality check (captured 2026-07-17, Claude Code 2.x): failing commands
/// arrive as `PostToolUseFailure` with `error: "Exit code N\n..."`;
/// `PostToolUse` fires only for successes and its `tool_response`
/// (`{stdout, stderr, interrupted, ...}`) carries no exit code. The
/// `PostToolUse` probe below is defensive, for future payload shapes.
fn detect_failure(payload: &HookPayload) -> Option<Option<i32>> {
    if payload.tool_name != "Bash" {
        return None;
    }
    // A user interrupt is not a failure worth investigating.
    if payload.is_interrupt {
        return None;
    }
    match payload.hook_event_name.as_str() {
        "PostToolUseFailure" => {
            let exit = payload.error.as_deref().and_then(parse_exit_code);
            Some(exit)
        }
        "PostToolUse" => {
            let obj = payload.tool_response.as_object()?;
            if obj.get("interrupted").and_then(|v| v.as_bool()) == Some(true) {
                return None;
            }
            for key in ["exit_code", "exitCode"] {
                if let Some(code) = obj.get(key).and_then(|v| v.as_i64()) {
                    if code != 0 {
                        return Some(Some(code as i32));
                    }
                    return None; // explicit success
                }
            }
            for key in ["is_error", "isError"] {
                if obj.get(key).and_then(|v| v.as_bool()) == Some(true) {
                    return Some(None);
                }
            }
            None
        }
        _ => None,
    }
}

/// Parse the leading `"Exit code N"` out of a `PostToolUseFailure` error.
fn parse_exit_code(error: &str) -> Option<i32> {
    let rest = error.strip_prefix("Exit code ")?;
    let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
    digits.parse().ok()
}

/// Build the context to inject, plus a fingerprint for rate limiting.
///
/// Three outcomes: a live summary naming the top error group (index
/// exists, query succeeded, groups found), a static pointer at the tool
/// (no index yet but `./logs/` has files — never trigger a first-ever
/// ingest from inside a hook), or `None` (nothing useful to say).
async fn build_context(exit_code: Option<i32>) -> Option<(String, String)> {
    let config = Config::load().ok()?;
    let has_index = config.index_dir().exists();
    let has_logs = logs_dir_has_files();

    if has_index {
        let query = tokio::task::spawn_blocking(query_recent_errors);
        match tokio::time::timeout(QUERY_BUDGET, query).await {
            Ok(Ok(Some((total, group)))) => {
                return Some(live_context(exit_code, total, &group));
            }
            // Query ran and found nothing in the window: the logs hold no
            // evidence, so stay silent rather than spend Claude's tokens.
            Ok(Ok(None)) => return None,
            // Timeout or query failure: degrade to the static pointer.
            _ => {}
        }
    }
    if has_index || has_logs {
        return Some(static_context(exit_code));
    }
    None
}

/// One `stat`-cheap check: does `./logs/` contain at least one file?
fn logs_dir_has_files() -> bool {
    std::fs::read_dir(Path::new("logs"))
        .map(|mut entries| entries.any(|e| e.map(|e| e.path().is_file()).unwrap_or(false)))
        .unwrap_or(false)
}

struct TopGroup {
    template: String,
    count: usize,
    last_seen: String,
    fingerprint: String,
}

/// Sync + query the index for recent error groups, in-process.
///
/// Deliberately NOT `ErrorsCommand::execute()` — that prints the v1
/// envelope to stdout, which would corrupt the hook response.
fn query_recent_errors() -> Option<(usize, TopGroup)> {
    let config = Config::load().ok()?;
    let (searcher, _report) = crate::cmd::prepare_read(config).ok()?;
    let options = SearchOptions {
        level: Some("error".to_string()),
        last: Some(LOOKBACK.to_string()),
        ..Default::default()
    };
    let result = searcher.errors_query(&options, TOP_GROUPS).ok()?;
    let top = result.groups.into_iter().next()?;
    Some((
        result.total_errors,
        TopGroup {
            template: top.template,
            count: top.count,
            last_seen: top.last_seen,
            fingerprint: top.fingerprint,
        },
    ))
}

fn live_context(exit_code: Option<i32>, total_errors: usize, top: &TopGroup) -> (String, String) {
    let context = format!(
        "The failed command{} may have runtime evidence in this project's logs: \
         {} indexed the failure window and found {} error entr{} in the last {} — \
         top group: \"{}\" ({}×, last seen {}). \
         Run `{} --quiet errors --last {}` for the full breakdown.",
        exit_phrase(exit_code),
        env!("CARGO_PKG_NAME"),
        total_errors,
        if total_errors == 1 { "y" } else { "ies" },
        LOOKBACK,
        truncate_chars(&top.template, MAX_TEMPLATE_CHARS),
        top.count,
        top.last_seen,
        env!("CARGO_PKG_NAME"),
        LOOKBACK,
    );
    (
        truncate_chars(&context, MAX_CONTEXT_CHARS),
        top.fingerprint.clone(),
    )
}

fn static_context(exit_code: Option<i32>) -> (String, String) {
    let context = format!(
        "The failed command{} may have left runtime evidence in this project's \
         log files. Run `{} --quiet errors --last {}` to check for related \
         errors (zero setup; exit 2 just means the logs hold nothing recent).",
        exit_phrase(exit_code),
        env!("CARGO_PKG_NAME"),
        LOOKBACK,
    );
    (
        truncate_chars(&context, MAX_CONTEXT_CHARS),
        "static".to_string(),
    )
}

fn exit_phrase(exit_code: Option<i32>) -> String {
    match exit_code {
        Some(code) => format!(" (exit code {})", code),
        None => String::new(),
    }
}

fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let cut: String = s.chars().take(max.saturating_sub(1)).collect();
        format!("{}…", cut)
    }
}

/// Suppress repeats: the same fingerprint fires at most once per
/// `RATE_LIMIT`; a *different* top group always fires immediately.
/// Records the fire as a side effect when not suppressed.
fn rate_limited(cwd: &str, fingerprint: &str) -> bool {
    let path = state_path(cwd);
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);

    if let Some(state) = std::fs::read_to_string(&path)
        .ok()
        .and_then(|raw| serde_json::from_str::<HookState>(&raw).ok())
    {
        let within_window = now.saturating_sub(state.last_fired_unix) < RATE_LIMIT.as_secs();
        if within_window && state.last_fingerprint == fingerprint {
            return true;
        }
    }

    let state = HookState {
        last_fired_unix: now,
        last_fingerprint: fingerprint.to_string(),
    };
    if let Ok(raw) = serde_json::to_string(&state) {
        let _ = std::fs::write(&path, raw);
    }
    false
}

/// Per-project state file in the OS temp dir (never in the project).
fn state_path(cwd: &str) -> std::path::PathBuf {
    let digest = sha2::Sha256::digest(cwd.as_bytes());
    std::env::temp_dir().join(format!(
        "{}-hook-{}.json",
        env!("CARGO_PKG_NAME"),
        hex::encode(&digest[..6])
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn failure_payload(command: &str, error: &str) -> HookPayload {
        HookPayload {
            hook_event_name: "PostToolUseFailure".to_string(),
            tool_name: "Bash".to_string(),
            cwd: "/tmp".to_string(),
            tool_input: ToolInput {
                command: command.to_string(),
            },
            error: Some(error.to_string()),
            ..Default::default()
        }
    }

    #[test]
    fn test_parse_exit_code() {
        assert_eq!(parse_exit_code("Exit code 3\nsome stderr"), Some(3));
        assert_eq!(parse_exit_code("Exit code 127"), Some(127));
        assert_eq!(parse_exit_code("something else"), None);
        assert_eq!(parse_exit_code("Exit code x"), None);
    }

    #[test]
    fn test_detect_failure_on_post_tool_use_failure() {
        let payload = failure_payload("cargo build", "Exit code 101\nerror[E0308]");
        assert_eq!(detect_failure(&payload), Some(Some(101)));
    }

    #[test]
    fn test_interrupt_is_not_a_failure() {
        let mut payload = failure_payload("sleep 100", "Exit code 130\n");
        payload.is_interrupt = true;
        assert_eq!(detect_failure(&payload), None);
    }

    #[test]
    fn test_non_bash_tools_are_ignored() {
        let mut payload = failure_payload("n/a", "Exit code 1\n");
        payload.tool_name = "Edit".to_string();
        assert_eq!(detect_failure(&payload), None);
    }

    #[test]
    fn test_post_tool_use_success_shape_is_silent() {
        // The real captured success payload: no exit code fields at all.
        let payload = HookPayload {
            hook_event_name: "PostToolUse".to_string(),
            tool_name: "Bash".to_string(),
            tool_response: serde_json::json!({
                "stdout": "hello", "stderr": "", "interrupted": false,
                "isImage": false, "noOutputExpected": false
            }),
            ..Default::default()
        };
        assert_eq!(detect_failure(&payload), None);
    }

    #[test]
    fn test_post_tool_use_with_explicit_exit_code_fires() {
        let mut payload = HookPayload {
            hook_event_name: "PostToolUse".to_string(),
            tool_name: "Bash".to_string(),
            ..Default::default()
        };
        payload.tool_response = serde_json::json!({"exit_code": 2});
        assert_eq!(detect_failure(&payload), Some(Some(2)));
        payload.tool_response = serde_json::json!({"exit_code": 0});
        assert_eq!(detect_failure(&payload), None);
    }

    #[test]
    fn test_truncate_chars_respects_char_boundaries() {
        assert_eq!(truncate_chars("héllo", 10), "héllo");
        let long = "é".repeat(700);
        let cut = truncate_chars(&long, 600);
        assert_eq!(cut.chars().count(), 600);
        assert!(cut.ends_with('…'));
    }

    #[test]
    fn test_context_is_capped() {
        let top = TopGroup {
            template: "x".repeat(1000),
            count: 42,
            last_seen: "2026-07-17T09:00:00Z".to_string(),
            fingerprint: "abc".to_string(),
        };
        let (context, fp) = live_context(Some(1), 42, &top);
        assert!(context.chars().count() <= MAX_CONTEXT_CHARS);
        assert_eq!(fp, "abc");
    }
}
