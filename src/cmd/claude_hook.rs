//! Claude Code hook entrypoint: `somethingsoff claude hook`.
//!
//! The bundled plugin (`claude-plugin/hooks/hooks.json`) runs this on two
//! tool events. Stdin is Claude Code's hook event JSON; stdout is an
//! optional hook response.
//!
//! - `PostToolUseFailure` (Bash) — after a command fails,
//!   `additionalContext` points Claude at the log evidence, ideally the
//!   actual top error group from the freshly synced index.
//! - `PreToolUse` (Bash or Read) — before a call that hand-reads an
//!   indexed log file (`cat logs/app.log`, or `Read` on the same path),
//!   `permissionDecision: deny` returns the indexed answer as the reason.
//!   The query has already run, so Claude gets the answer instead of a
//!   suggestion to go find it — and the answer is scoped to the source
//!   being read, so a loud neighbour can't speak for a quiet file.
//!
//! This command is exempt from the v1 output envelope (see
//! docs/CONTRACT.md): its stdout belongs to Claude Code's hook protocol.
//! A hook must never become noise, so every path that can't produce a
//! useful suggestion exits 0 silently — malformed payloads, missing state,
//! query errors, and timeouts included. For `PreToolUse` that rule is
//! stronger: silence means the command runs untouched, so a hook that
//! can't beat the raw file never blocks it.

use std::io::Read;
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use sha2::Digest;

use crate::config::Config;
use crate::index::searcher::SearchOptions;

/// Trace one decision to stderr when `SOMETHINGSOFF_HOOK_DEBUG` is set.
///
/// This hook exits 0 in silence on every path that has nothing useful to
/// say, which is right in production and leaves it undiagnosable: "no
/// errors in the window" and "broken since a payload change" look
/// identical from outside. Claude Code surfaces hook stderr under
/// `claude --debug`, so this is the supported way to ask it to explain
/// itself. Off by default — stderr noise is still noise.
macro_rules! trace {
    ($($arg:tt)*) => {
        if std::env::var_os("SOMETHINGSOFF_HOOK_DEBUG").is_some() {
            eprintln!("[somethingsoff hook] {}", format!($($arg)*));
        }
    };
}

/// Failing commands can embed sizable build output in the payload's
/// `error` field; accept real-world payloads but stay bounded.
const MAX_STDIN_BYTES: u64 = 8 * 1024 * 1024;
/// Time window the failure hook queries — and tells Claude to query.
const LOOKBACK: &str = "15m";
/// Window the `PreToolUse` answer summarises. Wider than `LOOKBACK`: a
/// `cat logs/app.log` isn't scoped to a command that just ran.
const PRE_LOOKBACK: &str = "1h";
/// Error groups fetched for the summary (only the top one is named).
const TOP_GROUPS: usize = 3;
/// Same-fingerprint suggestions are suppressed for this long.
const RATE_LIMIT: Duration = Duration::from_secs(300);
/// Budget for auto-sync + query; on overrun fall back to the static hint.
const QUERY_BUDGET: Duration = Duration::from_secs(3);
/// Ceiling for the injected context — hooks should be cheap in tokens.
const MAX_CONTEXT_CHARS: usize = 600;
/// Ceiling for a deny reason. Slightly wider than `MAX_CONTEXT_CHARS`
/// because the reason also carries the replacement commands and the
/// escape hatch; capping the template keeps it far below this in practice.
const MAX_REASON_CHARS: usize = 700;
/// Longest masked template quoted inside the context.
const MAX_TEMPLATE_CHARS: usize = 160;

/// Commands that dump a file's contents. `tail -f` is included on
/// purpose: following a log by hand is what `watch`/`tap` exist for.
const FILE_READERS: [&str; 11] = [
    "cat", "bat", "head", "tail", "less", "more", "grep", "egrep", "fgrep", "rg", "ag",
];
/// Readers whose first non-flag argument is a search pattern, not a path.
const PATTERN_FIRST_READERS: [&str; 5] = ["grep", "egrep", "fgrep", "rg", "ag"];
/// How many recent denials to remember. A single slot let an agent that
/// alternates phrasings (`cat X`, then `tail X`, then `cat X` again)
/// clobber its own escape hatch and be denied forever.
const PRE_DENY_MEMORY: usize = 16;
/// Below this many bytes, reading the file raw is simply the better
/// answer: the whole file costs less context than the summary of it and
/// tells Claude strictly more. Blocking here would spend tokens to lose
/// information — the exact opposite of the point.
const MIN_RAW_BYTES: u64 = 4096;

/// The subset of Claude Code's hook event payload the hook cares about.
/// Every field is optional-with-default: an unexpected shape must degrade
/// to silence, not to a parse error.
#[derive(Default)]
struct HookPayload {
    hook_event_name: String,
    tool_name: String,
    cwd: String,
    tool_input: ToolInput,
    /// The failure text, carrying `"Exit code N\n<output>"` when present.
    error: Option<String>,
    is_interrupt: bool,
    /// `PostToolUse` result object, probed for explicit failure signals.
    tool_response: serde_json::Value,
}

#[derive(Default)]
struct ToolInput {
    /// `Bash`.
    command: String,
    /// `Read` — Claude's own file reader, which needs no shell parsing.
    file_path: String,
}

/// Keys the failure text has been observed under. Taking the first that
/// holds a string means a rename on Claude Code's side costs the exit-code
/// annotation, not the whole hook.
const ERROR_KEYS: [&str; 3] = ["error", "error_message", "failure_reason"];

impl HookPayload {
    /// Build from the event JSON one field at a time.
    ///
    /// Deliberately not a derived `Deserialize`: that is all-or-nothing, so a
    /// single field arriving with an unexpected type — a future release
    /// sending `error` as an object rather than a string — would fail the
    /// whole parse and disable the hook permanently, with no signal at all.
    /// The struct's doc promises each field degrades on its own; only this
    /// makes that true.
    fn from_value(value: &serde_json::Value) -> Self {
        HookPayload {
            hook_event_name: string_at(value, "hook_event_name"),
            tool_name: string_at(value, "tool_name"),
            cwd: string_at(value, "cwd"),
            tool_input: ToolInput {
                command: nested_string(value, "tool_input", "command"),
                file_path: nested_string(value, "tool_input", "file_path"),
            },
            error: ERROR_KEYS
                .iter()
                .find_map(|key| value.get(key).and_then(|v| v.as_str()))
                .map(str::to_string),
            is_interrupt: value
                .get("is_interrupt")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false),
            tool_response: value.get("tool_response").cloned().unwrap_or_default(),
        }
    }
}

/// A top-level string field, or empty when absent or the wrong type.
fn string_at(value: &serde_json::Value, key: &str) -> String {
    value
        .get(key)
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string()
}

/// A string one level down, or empty when any step is absent or mistyped.
fn nested_string(value: &serde_json::Value, outer: &str, inner: &str) -> String {
    value
        .get(outer)
        .and_then(|v| v.get(inner))
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string()
}

/// Rate-limit state, kept in the OS temp dir (keyed by project cwd) so the
/// hook never creates `./.somethingsoff/` in a project on its own.
#[derive(Deserialize, Serialize, Default)]
#[serde(default)]
struct HookState {
    last_fired_unix: u64,
    last_fingerprint: String,
    /// `PreToolUse` denials, newest last, tracked separately so the two
    /// events sharing this file can never suppress each other.
    recent_pre_denies: Vec<PreDeny>,
}

#[derive(Deserialize, Serialize, Default, Clone)]
#[serde(default)]
struct PreDeny {
    fingerprint: String,
    at_unix: u64,
}

/// Entry point. Infallible by design: always exits 0, prints either one
/// hook-response JSON line or nothing.
pub async fn run() -> u8 {
    crate::set_quiet(true);

    let Some(payload) = read_payload() else {
        return 0;
    };
    trace!(
        "event={:?} tool={:?} cwd={:?}",
        payload.hook_event_name,
        payload.tool_name,
        payload.cwd
    );
    // Never act on our own invocations (including the skill's recipes).
    if payload.tool_input.command.contains(env!("CARGO_PKG_NAME")) {
        trace!("bail: the command is one of ours");
        return 0;
    }
    // All project state (config, index, logs dir) is cwd-relative.
    if payload.cwd.is_empty() || std::env::set_current_dir(&payload.cwd).is_err() {
        trace!("bail: cwd missing or unusable");
        return 0;
    }

    // `PreToolUse` also covers `Read`; the failure hook is Bash-only.
    if payload.hook_event_name == "PreToolUse" {
        return run_pre_tool_use(&payload).await;
    }
    if payload.tool_name != "Bash" {
        trace!("bail: the failure hook is Bash-only");
        return 0;
    }
    run_post_failure(&payload).await
}

/// After a failed command: point Claude at the evidence.
async fn run_post_failure(payload: &HookPayload) -> u8 {
    let Some(exit_code) = detect_failure(payload) else {
        trace!("bail: not a recognised failure");
        return 0;
    };
    let Some((context, fingerprint)) = build_context(exit_code).await else {
        trace!("bail: nothing worth injecting");
        return 0;
    };
    if rate_limited(&payload.cwd, &fingerprint) {
        trace!("bail: rate-limited on fingerprint {fingerprint} (window {RATE_LIMIT:?})");
        return 0;
    }
    trace!("injecting additionalContext");
    emit(
        &payload.hook_event_name,
        serde_json::json!({ "additionalContext": context }),
    );
    0
}

/// Before a call that hand-reads an indexed log: answer from the index
/// instead of letting the raw file into the context window.
///
/// The rule is deliberately narrow — a reader (`cat`, `tail`, `grep`, … or
/// Claude's own `Read`) pointed at a path this project *actually* indexes.
/// Everything else runs untouched, and a deny is only emitted when the
/// index has something better to say than the file does.
async fn run_pre_tool_use(payload: &HookPayload) -> u8 {
    // Cheapest first: this runs before *every* Bash call Claude makes, so
    // the common "not a log read at all" case must cost no filesystem I/O.
    let (candidates, dedup_key) = match payload.tool_name.as_str() {
        "Bash" => (
            reader_path_tokens(&payload.tool_input.command),
            payload.tool_input.command.as_str(),
        ),
        "Read" => (
            vec![payload.tool_input.file_path.as_str()],
            payload.tool_input.file_path.as_str(),
        ),
        _ => return 0,
    };
    if dedup_key.is_empty() || candidates.is_empty() {
        return 0;
    }

    let Ok(config) = Config::load() else {
        return 0;
    };
    // Same rule as the failure hook: never trigger a first-ever ingest
    // from inside a hook. With no index there is nothing to offer.
    if !config.index_dir().exists() {
        return 0;
    }
    let sources = indexed_sources(&config);
    let Some(target) = candidates
        .iter()
        .find_map(|candidate| resolve_target(candidate, &sources))
    else {
        return 0;
    };
    // Only block a read the answer can actually beat. A small file is
    // cheaper to read whole than to summarise — the exception being a
    // follow, which never ends no matter how small the file is today.
    let follows = payload.tool_name == "Bash" && follows_a_file(&payload.tool_input.command);
    if !follows && indexed_bytes(&target.paths) < MIN_RAW_BYTES {
        return 0;
    }
    let Some(reason) = deny_reason(&target).await else {
        return 0;
    };
    // A repeat of the same read is always allowed through: having seen the
    // indexed answer, Claude may still want the raw bytes, and a hook must
    // never trap the agent in a deny loop.
    if pre_deny_suppressed(&payload.cwd, dedup_key) {
        return 0;
    }
    emit(
        &payload.hook_event_name,
        serde_json::json!({
            "permissionDecision": "deny",
            "permissionDecisionReason": reason,
        }),
    );
    0
}

/// One hook response line on stdout: `fields` merged into
/// `hookSpecificOutput` alongside the event name.
fn emit(event: &str, fields: serde_json::Value) {
    let mut output = serde_json::Map::new();
    output.insert(
        "hookEventName".to_string(),
        serde_json::Value::String(event.to_string()),
    );
    if let serde_json::Value::Object(map) = fields {
        output.extend(map);
    }
    println!("{}", serde_json::json!({ "hookSpecificOutput": output }));
}

fn read_payload() -> Option<HookPayload> {
    let mut raw = String::new();
    std::io::stdin()
        .lock()
        .take(MAX_STDIN_BYTES)
        .read_to_string(&mut raw)
        .ok()?;
    let value: serde_json::Value = serde_json::from_str(&raw)
        .inspect_err(|e| trace!("stdin was not valid JSON: {e}"))
        .ok()?;
    Some(HookPayload::from_value(&value))
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
    let config = Config::load()
        .inspect_err(|e| trace!("Config::load failed: {e}"))
        .ok()?;
    let has_index = config.index_dir().exists();
    let has_logs = logs_dir_has_files();
    trace!("has_index={has_index} has_logs={has_logs}");

    if has_index {
        let query = tokio::task::spawn_blocking(|| query_recent_errors(LOOKBACK, None));
        match tokio::time::timeout(QUERY_BUDGET, query).await {
            Ok(Ok(Some((total, group)))) => {
                trace!("live context: {total} errors in {LOOKBACK}");
                return Some(live_context(exit_code, total, &group));
            }
            // Query ran and found nothing in the window: the logs hold no
            // evidence, so stay silent rather than spend Claude's tokens.
            Ok(Ok(None)) => {
                trace!("no errors in the last {LOOKBACK}");
                return None;
            }
            // Timeout or query failure: degrade to the static pointer.
            Ok(Err(e)) => trace!("query task failed: {e}"),
            Err(_) => trace!("query exceeded its {QUERY_BUDGET:?} budget"),
        }
    }
    if has_index || has_logs {
        trace!("static context");
        return Some(static_context(exit_code));
    }
    trace!("no index and no ./logs — nothing to say");
    None
}

/// One `stat`-cheap check: does `./logs/` contain at least one file?
fn logs_dir_has_files() -> bool {
    std::fs::read_dir(Path::new("logs"))
        .map(|mut entries| entries.any(|e| e.map(|e| e.path().is_file()).unwrap_or(false)))
        .unwrap_or(false)
}

/// A read this hook can answer for: the path as the agent wrote it, plus
/// the indexed source it maps to.
struct IndexedTarget {
    /// Echoed back in the deny reason, so Claude sees its own path.
    display: String,
    /// The one source being read. `None` for a directory-shaped read
    /// (`grep -r boom logs/`), where the whole-index answer is correct.
    source: Option<String>,
    /// Canonical paths this read covers, for weighing the raw bytes it
    /// would pull against the cost of the answer.
    paths: Vec<String>,
}

/// The `(source name, canonical path)` pairs this project actually
/// indexes, from the same discovery the sync engine uses.
///
/// Asking discovery — rather than pattern-matching the string — is what
/// keeps the hook from claiming `/var/log/system.log` or `npm-debug.log`
/// is ours and answering with unrelated project errors.
fn indexed_sources(config: &Config) -> Vec<(String, String)> {
    let state = crate::sync::state::SyncState::load(&crate::config::base_dir());
    crate::sync::discover::discover_sources(config, &state)
        .into_iter()
        .map(|(name, path)| (name, crate::sync::state::canonical_key(&path)))
        .collect()
}

/// The path-shaped arguments given to reader programs in this command
/// line, in the order they appear.
///
/// Purely lexical, so a command that names no reader costs nothing. Word
/// splitting is intentionally naive: the goal is to recognise the handful
/// of shapes an agent actually types — `cat logs/app.log`, `tail -n 200
/// logs/app.log`, `grep -r boom logs/` — not to parse bash. Anything
/// unrecognised falls through and runs, which is the safe way to be wrong.
fn reader_path_tokens(command: &str) -> Vec<&str> {
    let mut tokens_out = Vec::new();
    for segment in split_segments(command) {
        let mut tokens = segment.split_whitespace();
        let Some(program) = tokens.next() else {
            continue;
        };
        // `/bin/cat` and `cat` are the same command.
        let program = program.rsplit('/').next().unwrap_or(program);
        if !FILE_READERS.contains(&program) {
            continue;
        }
        // For `grep`-likes the first non-flag word is the pattern, not a
        // path. Without this, `grep -rn 'logs/app.log' src/` — a search
        // through *source code* for that string — gets denied.
        let mut pattern_pending = PATTERN_FIRST_READERS.contains(&program);
        for token in tokens {
            if token.starts_with('-') {
                continue;
            }
            if pattern_pending {
                pattern_pending = false;
                continue;
            }
            tokens_out.push(token);
        }
    }
    tokens_out
}

/// Split on the shell operators that separate commands, so a reader
/// hidden behind a pipe or `&&` is still seen.
fn split_segments(command: &str) -> Vec<&str> {
    command
        .split(['|', ';', '\n'])
        .flat_map(|segment| segment.split("&&"))
        .collect()
}

/// Map one path token onto the index. Three outcomes: an exact indexed
/// file, a directory holding indexed files, or `None` — not ours, so let
/// the read run untouched.
fn resolve_target(candidate: &str, sources: &[(String, String)]) -> Option<IndexedTarget> {
    let candidate = candidate.trim_matches(['"', '\'']);
    if candidate.is_empty() {
        return None;
    }
    let mut normalized = candidate;
    while let Some(rest) = normalized.strip_prefix("./") {
        normalized = rest;
    }
    let key = crate::sync::state::canonical_key(Path::new(normalized));

    if let Some((name, path)) = sources.iter().find(|(_, path)| *path == key) {
        return Some(IndexedTarget {
            display: candidate.to_string(),
            source: Some(name.clone()),
            paths: vec![path.clone()],
        });
    }

    // A directory of indexed files, or a glob inside one. Deliberately
    // matched against each source's *immediate* parent: `grep -r boom
    // logs/` is ours, `grep -r boom .` is an ordinary code search.
    let dir = if normalized.contains('*') || normalized.contains('?') {
        let parent = Path::new(normalized).parent()?;
        if parent.as_os_str().is_empty() {
            return None;
        }
        crate::sync::state::canonical_key(parent)
    } else {
        key
    };
    let dir = dir.trim_end_matches('/');
    let held: Vec<String> = sources
        .iter()
        .filter(|(_, path)| {
            Path::new(path)
                .parent()
                .and_then(|p| p.to_str())
                .is_some_and(|p| p == dir)
        })
        .map(|(_, path)| path.clone())
        .collect();
    (!held.is_empty()).then(|| IndexedTarget {
        display: candidate.to_string(),
        source: None,
        paths: held,
    })
}

/// Total bytes the read would pull in. Missing files count as zero, which
/// biases toward letting the command run — the safe way to be wrong.
fn indexed_bytes(paths: &[String]) -> u64 {
    paths
        .iter()
        .filter_map(|path| std::fs::metadata(path).ok())
        .map(|meta| meta.len())
        .sum()
}

/// Does this command *follow* a file (`tail -f`)? Worth intercepting at
/// any size, because it never returns on its own — watching a log by hand
/// is what `watch`/`tap` exist for.
fn follows_a_file(command: &str) -> bool {
    split_segments(command).iter().any(|segment| {
        let mut tokens = segment.split_whitespace();
        let Some(program) = tokens.next() else {
            return false;
        };
        let program = program.rsplit('/').next().unwrap_or(program);
        if !matches!(program, "tail" | "less" | "more") {
            return false;
        }
        tokens.any(|token| {
            token == "--follow"
                || token == "--retry"
                || (token.starts_with('-')
                    && !token.starts_with("--")
                    && token.chars().any(|c| c.eq_ignore_ascii_case(&'f')))
        })
    })
}

/// The indexed answer, or `None` when there isn't one worth blocking for
/// (nothing in the window, or the query blew its budget).
///
/// The query is scoped to the source being read. Answering a `cat
/// logs/auth.json` with the whole index's top group is worse than
/// silence: it buries the file's own errors under a louder neighbour's.
async fn deny_reason(target: &IndexedTarget) -> Option<String> {
    let source = target.source.clone();
    let query =
        tokio::task::spawn_blocking(move || query_recent_errors(PRE_LOOKBACK, source.as_deref()));
    let (total, top) = match tokio::time::timeout(QUERY_BUDGET, query).await {
        Ok(Ok(Some(found))) => found,
        _ => return None,
    };

    let name = env!("CARGO_PKG_NAME");
    let scope = match &target.source {
        Some(_) => "from this file",
        None => "from these logs",
    };
    // Point the follow-up at the same slice the summary describes.
    let raw_lines = match &target.source {
        Some(source) => format!(
            "{} --quiet search --last {} --source {} -n 50 --compact",
            name, PRE_LOOKBACK, source
        ),
        None => format!(
            "{} --quiet search --last {} -n 50 --compact",
            name, PRE_LOOKBACK
        ),
    };
    let reason = format!(
        "`{}` is indexed by {}; reading it raw floods the context window. \
         Already queried for you — last {} {}: {} error entr{}, \
         top group \"{}\" ({}×, last seen {}). \
         Full breakdown: `{} --quiet errors --last {}`. \
         Bounded raw lines: `{}`. \
         Re-run this exact command to read the file anyway.",
        target.display,
        name,
        PRE_LOOKBACK,
        scope,
        total,
        if total == 1 { "y" } else { "ies" },
        truncate_chars(&top.template, MAX_TEMPLATE_CHARS),
        top.count,
        top.last_seen,
        name,
        PRE_LOOKBACK,
        raw_lines,
    );
    Some(truncate_chars(&reason, MAX_REASON_CHARS))
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
fn query_recent_errors(lookback: &str, source: Option<&str>) -> Option<(usize, TopGroup)> {
    let config = Config::load().ok()?;
    let (searcher, _report) = crate::cmd::prepare_read(config).ok()?;
    let options = SearchOptions {
        level: Some("error".to_string()),
        last: Some(lookback.to_string()),
        source: source.map(str::to_string),
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
    let mut state = load_state(&path);
    let now = now_unix();

    let within_window = now.saturating_sub(state.last_fired_unix) < RATE_LIMIT.as_secs();
    if within_window && state.last_fingerprint == fingerprint {
        return true;
    }

    state.last_fired_unix = now;
    state.last_fingerprint = fingerprint.to_string();
    save_state(&path, &state);
    false
}

/// A given read is denied at most once per `RATE_LIMIT`; the retry runs.
/// Records the deny as a side effect when not suppressed.
///
/// The last `PRE_DENY_MEMORY` denials are remembered, not just the most
/// recent. With one slot, an agent that tried `cat X`, then `tail X`, then
/// `cat X` again found its own escape hatch overwritten — and two
/// alternating phrasings were denied forever.
fn pre_deny_suppressed(cwd: &str, read: &str) -> bool {
    let path = state_path(cwd);
    let mut state = load_state(&path);
    let now = now_unix();
    let fingerprint = hex::encode(&sha2::Sha256::digest(read.as_bytes())[..8]);

    state
        .recent_pre_denies
        .retain(|d| now.saturating_sub(d.at_unix) < RATE_LIMIT.as_secs());
    if state
        .recent_pre_denies
        .iter()
        .any(|d| d.fingerprint == fingerprint)
    {
        return true;
    }

    state.recent_pre_denies.push(PreDeny {
        fingerprint,
        at_unix: now,
    });
    let overflow = state
        .recent_pre_denies
        .len()
        .saturating_sub(PRE_DENY_MEMORY);
    state.recent_pre_denies.drain(..overflow);
    save_state(&path, &state);
    false
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Read-modify-write, so hooks for different events can share one file
/// without clobbering each other's fields. A missing or corrupt file is
/// indistinguishable from "no state yet".
fn load_state(path: &Path) -> HookState {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_default()
}

fn save_state(path: &Path, state: &HookState) {
    if let Ok(raw) = serde_json::to_string(state) {
        let _ = std::fs::write(path, raw);
    }
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
                ..Default::default()
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

    /// A single mistyped field must not disable the hook.
    ///
    /// With a derived `Deserialize` this whole payload failed to parse, so
    /// the hook went permanently silent with no signal — the worst outcome
    /// for something whose normal behaviour is also silence. Each field has
    /// to degrade on its own.
    #[test]
    fn test_one_mistyped_field_does_not_discard_the_payload() {
        let value = serde_json::json!({
            "hook_event_name": "PostToolUseFailure",
            "tool_name": "Bash",
            "cwd": "/tmp/x",
            "tool_input": {"command": "npm test"},
            "error": 123,                 // string everywhere else
            "is_interrupt": "no",         // bool everywhere else
            "tool_response": {"exit_code": 3},
        });
        let payload = HookPayload::from_value(&value);
        // The mistyped fields fall back...
        assert_eq!(payload.error, None);
        assert!(!payload.is_interrupt);
        // ...and everything else still arrives intact.
        assert_eq!(payload.hook_event_name, "PostToolUseFailure");
        assert_eq!(payload.tool_name, "Bash");
        assert_eq!(payload.cwd, "/tmp/x");
        assert_eq!(payload.tool_input.command, "npm test");
        assert_eq!(detect_failure(&payload), Some(None));
    }

    /// `tool_input` arriving as a scalar must not panic or poison the parse.
    #[test]
    fn test_wrongly_shaped_tool_input_degrades_to_empty() {
        let value = serde_json::json!({
            "hook_event_name": "PreToolUse",
            "tool_name": "Bash",
            "cwd": "/tmp/x",
            "tool_input": "not an object",
        });
        let payload = HookPayload::from_value(&value);
        assert_eq!(payload.tool_input.command, "");
        assert_eq!(payload.tool_input.file_path, "");
        assert_eq!(payload.hook_event_name, "PreToolUse");
    }

    /// The failure text has been seen under more than one key; accept any of
    /// them so a rename costs the exit-code annotation, not the hook.
    #[test]
    fn test_failure_text_accepted_under_any_known_key() {
        for key in ERROR_KEYS {
            let value = serde_json::json!({
                "hook_event_name": "PostToolUseFailure",
                "tool_name": "Bash",
                key: "Exit code 42\nboom",
            });
            let payload = HookPayload::from_value(&value);
            assert_eq!(
                detect_failure(&payload),
                Some(Some(42)),
                "exit code should be read from {key}"
            );
        }
    }

    /// Missing fields are absent, not defaults that look like real data.
    #[test]
    fn test_empty_payload_is_inert() {
        let payload = HookPayload::from_value(&serde_json::json!({}));
        assert_eq!(payload.hook_event_name, "");
        assert_eq!(payload.tool_name, "");
        assert_eq!(detect_failure(&payload), None);
    }

    /// Sources as `indexed_sources` produces them: canonical paths, so
    /// both sides of the comparison go through the same normalisation.
    fn fake_sources(paths: &[(&str, &str)]) -> Vec<(String, String)> {
        paths
            .iter()
            .map(|(name, path)| {
                (
                    name.to_string(),
                    crate::sync::state::canonical_key(Path::new(path)),
                )
            })
            .collect()
    }

    fn app_log() -> Vec<(String, String)> {
        fake_sources(&[("app", "logs/app.log")])
    }

    /// The two production steps a `PreToolUse` Bash event runs — extract
    /// path tokens, then ground them in the real sources — as one call.
    fn reads_indexed_logs(command: &str, sources: &[(String, String)]) -> Option<IndexedTarget> {
        reader_path_tokens(command)
            .into_iter()
            .find_map(|token| resolve_target(token, sources))
    }

    #[test]
    fn test_detects_hand_reading_of_indexed_logs() {
        let sources = app_log();
        for command in [
            "cat logs/app.log",
            "cat ./logs/app.log",
            "tail -n 200 logs/app.log",
            "tail -f logs/app.log",
            "grep -r boom logs/",
            "less logs/app.log",
            "/bin/cat logs/app.log",
            "rg timeout logs/app.log",
            "cat \"logs/app.log\"",
        ] {
            assert!(
                reads_indexed_logs(command, &sources).is_some(),
                "should intercept: {command}"
            );
        }
    }

    #[test]
    fn test_intercepted_read_names_its_source() {
        let target = reads_indexed_logs("tail -f logs/app.log", &app_log()).unwrap();
        assert_eq!(target.display, "logs/app.log");
        // Scoping the query to this source is what keeps the answer honest.
        assert_eq!(target.source.as_deref(), Some("app"));
    }

    #[test]
    fn test_directory_read_answers_across_the_index() {
        let target = reads_indexed_logs("grep -r boom logs/", &app_log()).unwrap();
        assert_eq!(target.source, None);
    }

    #[test]
    fn test_ordinary_commands_run_untouched() {
        let sources = app_log();
        for command in [
            "cat README.md",
            "cat package.json", // a .json file is not a log
            "cat src/main.rs",
            "grep -r todo src/",
            "head -5 Cargo.toml",
            "npm test",
            "echo hi > logs/app.log", // writing, not reading
            "grep -r boom .",         // an ordinary code search, not ours
        ] {
            assert!(
                reads_indexed_logs(command, &sources).is_none(),
                "should run untouched: {command}"
            );
        }
    }

    /// Regression: a lexical `.log`/`.jsonl` rule denied any path that
    /// merely looked like a log, then answered with *this* project's
    /// errors — confidently wrong about a file it had never indexed.
    #[test]
    fn test_log_shaped_paths_we_do_not_index_run_untouched() {
        let sources = app_log();
        for command in [
            "cat /var/log/system.log",
            "cat npm-debug.log",
            "tail -100 /tmp/build.log",
            "cat node_modules/.cache/x.log",
            "cat logs/app.log.1", // rotated-out, no longer a source
        ] {
            assert!(
                reads_indexed_logs(command, &sources).is_none(),
                "not indexed, must run untouched: {command}"
            );
        }
    }

    /// Regression: for `grep`-likes the first non-flag word is the
    /// pattern. Searching source code *for* a log path is not a log read.
    #[test]
    fn test_a_log_path_used_as_a_search_pattern_is_not_a_read() {
        let sources = app_log();
        assert!(reads_indexed_logs("grep -rn 'logs/app.log' src/", &sources).is_none());
        assert!(reads_indexed_logs("rg logs/app.log --files-with-matches", &sources).is_none());
        // …but the same reader pointed at the file still fires.
        assert!(reads_indexed_logs("grep ERROR logs/app.log", &sources).is_some());
    }

    #[test]
    fn test_reader_behind_a_pipe_or_conjunction_is_seen() {
        let sources = app_log();
        assert!(reads_indexed_logs("npm test && cat logs/app.log", &sources).is_some());
        assert!(reads_indexed_logs("ls -la | grep x; tail logs/app.log", &sources).is_some());
    }

    #[test]
    fn test_configured_source_outside_logs_dir_counts() {
        let sources = fake_sources(&[("api", "var/output.txt")]);
        let target = reads_indexed_logs("cat var/output.txt", &sources).unwrap();
        assert_eq!(target.display, "var/output.txt");
        assert_eq!(target.source.as_deref(), Some("api"));
        assert!(reads_indexed_logs("cat var/other.txt", &sources).is_none());
    }

    #[test]
    fn test_follow_mode_is_recognised() {
        for command in [
            "tail -f logs/app.log",
            "tail -F logs/app.log",
            "tail --follow logs/app.log",
            "tail -n 50 -f logs/app.log",
        ] {
            assert!(follows_a_file(command), "should be a follow: {command}");
        }
        for command in [
            "tail -n 50 logs/app.log",
            "cat logs/app.log",
            "grep -f patterns.txt logs/app.log", // -f on grep is a file, not follow
        ] {
            assert!(!follows_a_file(command), "not a follow: {command}");
        }
    }

    #[test]
    fn test_read_tool_paths_resolve_the_same_way() {
        let sources = app_log();
        assert!(resolve_target("logs/app.log", &sources).is_some());
        assert!(resolve_target("README.md", &sources).is_none());
        assert!(resolve_target("", &sources).is_none());
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
