//! Source discovery: which log files should be synced, computed fresh on
//! every invocation (so files that appear after startup are picked up).
//!
//! Union, in precedence order (first name/path registered wins):
//! 1. `config.log_sources` (explicit configuration)
//! 2. `./logs/**` and `./log/**` (zero-config convention, recursive; source =
//!    file stem, path-qualified when nested)
//! 3. `<base_dir>/streams/*.jsonl` (tap journals, source = file stem)
//! 4. Files already tracked in state.json (e.g. explicitly `ingest`ed files)

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use crate::config::Config;
use crate::sync::state::{canonical_key, SyncState};

/// Primary directory scanned for zero-config log discovery.
pub const LOGS_DIR: &str = "logs";
/// Directories scanned, in order. `logs/` is the common convention; `log/` is
/// Rails', Django's and Java's — and skipping it meant the single largest log
/// file in a typical tree (a Rails `log/development.log`) was invisible.
pub const LOG_DIRS: [&str; 2] = ["logs", "log"];
/// Subdirectory of base_dir holding tap journals.
pub const STREAMS_DIR: &str = "streams";

/// Extensions treated as logs inside a dedicated log directory. `txt`/`out`/
/// `err` are included only because the directory itself is the evidence: a
/// `.txt` under `logs/` is a redirected stream, not prose.
const LOG_EXTENSIONS: [&str; 7] = ["log", "json", "jsonl", "ndjson", "txt", "out", "err"];

/// Rotation suffixes we cannot read. `app.log.1` is a log; `app.log.gz` has
/// the same shape and is a binary blob that would index as garbage.
const COMPRESSED_SUFFIXES: [&str; 6] = ["gz", "bz2", "xz", "zip", "zst", "7z"];

/// How deep to recurse inside a log directory. Real layouts nest by run or by
/// service (`logs/dep-3/build_closure.log`), which is one or two levels; the
/// bound exists mainly so a symlink cycle becomes a stopped walk rather than
/// an infinite one, since following symlinks into `logs/` is a documented
/// escape hatch.
const MAX_SCAN_DEPTH: usize = 4;

/// Ceiling on files taken from one directory tree. A pathological directory
/// should degrade to "indexed a lot" rather than hang the CLI.
const MAX_FILES_PER_DIR: usize = 512;

/// Discover all (source, path) pairs to sync. Paths are not required to
/// exist (missing files are skipped by the sync engine).
pub fn discover_sources(config: &Config, state: &SyncState) -> Vec<(String, PathBuf)> {
    let mut sources: Vec<(String, PathBuf)> = Vec::new();
    let mut seen_paths: HashSet<String> = HashSet::new();

    let mut push = |source: String, path: PathBuf, seen: &mut HashSet<String>| {
        if seen.insert(canonical_key(&path)) {
            sources.push((source, path));
        }
    };

    // 1. Explicit config sources (highest precedence for naming)
    for (name, path) in &config.log_sources {
        push(name.clone(), PathBuf::from(path), &mut seen_paths);
    }

    // 2. Zero-config scan of the conventional log directories, recursively.
    for dir in LOG_DIRS {
        let root = Path::new(dir);
        for path in scan_dir(root, &LOG_EXTENSIONS) {
            if let Some(name) = source_name(root, &path) {
                push(name, path.clone(), &mut seen_paths);
            }
        }
    }

    // 2b. Root-level `*.log` (`./dev.log`, `./npm-debug.log`), non-recursive.
    //
    // Deliberately narrower than the directory scan: inside `logs/` the
    // directory is evidence enough to trust a `.txt` or a `.json`, but at the
    // project root that same rule would index `package.json` and every README
    // adjacent text file. Here the `.log` extension carries the whole burden
    // of proof, and nothing else qualifies.
    for path in scan_shallow(Path::new("."), &["log"]) {
        if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
            push(stem.to_string(), path.clone(), &mut seen_paths);
        }
    }

    // 3. Tap journals (flat by construction)
    let streams_dir = crate::config::base_dir().join(STREAMS_DIR);
    for path in scan_dir(&streams_dir, &["jsonl"]) {
        if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
            push(stem.to_string(), path.clone(), &mut seen_paths);
        }
    }

    // 4. Previously tracked files (keeps explicitly-ingested files fresh)
    for (key, file_state) in &state.files {
        let path = PathBuf::from(key);
        push(file_state.source.clone(), path, &mut seen_paths);
    }

    // Deterministic order regardless of HashMap iteration
    sources.sort_by(|a, b| a.1.cmp(&b.1));
    sources
}

/// Collect log files under `dir`, recursing to `MAX_SCAN_DEPTH`.
///
/// Sorted, so discovery order is stable across filesystems.
fn scan_dir(dir: &Path, extensions: &[&str]) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    scan_into(dir, extensions, 0, &mut paths);
    paths.sort();
    paths
}

/// Collect log files directly inside `dir`, without recursing.
fn scan_shallow(dir: &Path, extensions: &[&str]) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    scan_into(dir, extensions, MAX_SCAN_DEPTH, &mut paths);
    paths.sort();
    paths
}

fn scan_into(dir: &Path, extensions: &[&str], depth: usize, out: &mut Vec<PathBuf>) {
    if depth > MAX_SCAN_DEPTH || out.len() >= MAX_FILES_PER_DIR || !dir.is_dir() {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    // Collect first so subdirectories are walked in a stable order.
    let mut children: Vec<PathBuf> = entries.flatten().map(|e| e.path()).collect();
    children.sort();
    for path in children {
        if out.len() >= MAX_FILES_PER_DIR {
            return;
        }
        // `is_dir`/`is_file` follow symlinks, which is deliberate: pointing a
        // symlink at a log outside the project is a supported way to index it.
        if path.is_dir() {
            scan_into(&path, extensions, depth + 1, out);
        } else if path.is_file() && is_log_file(&path, extensions) {
            out.push(path);
        }
    }
}

/// Does this filename look like a readable log?
fn is_log_file(path: &Path, extensions: &[&str]) -> bool {
    let Some(ext) = path.extension().and_then(|e| e.to_str()) else {
        return false;
    };
    if extensions.contains(&ext) {
        return true;
    }
    // Rotated: logrotate appends its own suffix, leaving the real extension
    // in the middle (`app.log.1`, `app.log.2026-08-09`). Skipped entirely when
    // the suffix marks a compressed archive, which we cannot read.
    if COMPRESSED_SUFFIXES.contains(&ext.to_ascii_lowercase().as_str()) {
        return false;
    }
    rotated_base(path).is_some_and(|base| {
        Path::new(&base)
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(|inner| extensions.contains(&inner))
    })
}

/// For `app.log.1`, the `app.log` underneath the rotation suffix.
fn rotated_base(path: &Path) -> Option<String> {
    let name = path.file_name()?.to_str()?;
    let (base, suffix) = name.rsplit_once('.')?;
    // Only a numeric or date-shaped suffix is a rotation; `.tar` is not.
    let rotated = suffix.chars().all(|c| c.is_ascii_digit() || c == '-');
    (rotated && !suffix.is_empty()).then(|| base.to_string())
}

/// Source label for a discovered file.
///
/// A top-level file keeps its stem (`logs/app.log` -> `app`), so existing
/// projects see no renaming. A nested one is named by its path relative to the
/// log directory (`logs/dep-1/build_closure.log` -> `dep-1/build_closure`),
/// because two runs each holding an `app.log` would otherwise collapse into
/// one indistinguishable `--source app`. Rotated files share the live file's
/// name: `app.log.1` is the same logical source as `app.log`.
fn source_name(root: &Path, path: &Path) -> Option<String> {
    let relative = path.strip_prefix(root).unwrap_or(path);
    let file_name = relative.file_name()?.to_str()?;
    let stem = match rotated_base(Path::new(file_name)) {
        Some(base) => Path::new(&base).file_stem()?.to_str()?.to_string(),
        None => Path::new(file_name).file_stem()?.to_str()?.to_string(),
    };
    match relative.parent().filter(|p| !p.as_os_str().is_empty()) {
        Some(parent) => Some(format!("{}/{}", parent.to_str()?, stem)),
        None => Some(stem),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sync::state::FileState;

    fn empty_config() -> Config {
        Config {
            general: Default::default(),
            log_sources: Default::default(),
            output: Default::default(),
            sync: Default::default(),
        }
    }

    #[test]
    fn test_config_sources_take_naming_precedence() {
        let dir = tempfile::TempDir::new().unwrap();
        let log = dir.path().join("app.log");
        std::fs::write(&log, "x\n").unwrap();

        let mut config = empty_config();
        config
            .log_sources
            .insert("backend".to_string(), log.to_string_lossy().to_string());

        let mut state = SyncState::default();
        // Same file also tracked in state under a different name — config wins.
        state.insert(
            &log,
            FileState {
                source: "old-name".to_string(),
                offset: 0,
                size: 0,
                mtime_ms: None,
                fingerprint: String::new(),
                last_ingested_at: String::new(),
            },
        );

        let sources = discover_sources(&config, &state);
        assert_eq!(sources.len(), 1);
        assert_eq!(sources[0].0, "backend");
    }

    #[test]
    fn test_state_files_are_rediscovered() {
        let dir = tempfile::TempDir::new().unwrap();
        let log = dir.path().join("custom.log");
        std::fs::write(&log, "x\n").unwrap();

        let mut state = SyncState::default();
        state.insert(
            &log,
            FileState {
                source: "custom".to_string(),
                offset: 2,
                size: 2,
                mtime_ms: None,
                fingerprint: String::new(),
                last_ingested_at: String::new(),
            },
        );

        let sources = discover_sources(&empty_config(), &state);
        assert_eq!(sources.len(), 1);
        assert_eq!(sources[0].0, "custom");
    }
}
