//! Source discovery: which log files should be synced, computed fresh on
//! every invocation (so files that appear after startup are picked up).
//!
//! Union, in precedence order (first name/path registered wins):
//! 1. `config.log_sources` (explicit configuration)
//! 2. `./logs/*.{log,json,jsonl}` (zero-config convention, source = file stem)
//! 3. `<base_dir>/streams/*.jsonl` (tap journals, source = file stem)
//! 4. Files already tracked in state.json (e.g. explicitly `ingest`ed files)

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use crate::config::Config;
use crate::sync::state::{canonical_key, SyncState};

/// Directory scanned for zero-config log discovery.
pub const LOGS_DIR: &str = "logs";
/// Subdirectory of base_dir holding tap journals.
pub const STREAMS_DIR: &str = "streams";

const LOG_EXTENSIONS: [&str; 3] = ["log", "json", "jsonl"];

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

    // 2. Zero-config ./logs scan
    for path in scan_dir(Path::new(LOGS_DIR), &LOG_EXTENSIONS) {
        if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
            push(stem.to_string(), path.clone(), &mut seen_paths);
        }
    }

    // 3. Tap journals
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

fn scan_dir(dir: &Path, extensions: &[&str]) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    if !dir.is_dir() {
        return paths;
    }
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_file() {
                continue;
            }
            let matches = path
                .extension()
                .and_then(|e| e.to_str())
                .is_some_and(|ext| extensions.contains(&ext));
            if matches {
                paths.push(path);
            }
        }
    }
    paths
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
