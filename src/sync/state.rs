//! Persistent sync state: per-file read cursors stored in `<base_dir>/state.json`.
//!
//! The state file is disposable by design: deduplication via `log_id` upserts
//! means a lost or corrupt state file only causes re-ingestion from offset 0,
//! which is correct (just slower). Loading therefore never fails — any read or
//! parse problem yields an empty state.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

/// Read cursor and freshness metadata for a single tracked log file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileState {
    /// Source name this file is ingested under
    pub source: String,
    /// Byte offset of the next unread byte
    pub offset: u64,
    /// File size at last sync (for cheap freshness checks)
    pub size: u64,
    /// File mtime in milliseconds since epoch at last sync
    pub mtime_ms: Option<u64>,
    /// Fingerprint of the file head (for rotation detection)
    pub fingerprint: String,
    /// When this file was last ingested (RFC3339)
    pub last_ingested_at: String,
}

/// All tracked file cursors, keyed by canonicalized absolute path.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SyncState {
    #[serde(default)]
    pub version: u32,
    #[serde(default)]
    pub files: BTreeMap<String, FileState>,
}

pub const STATE_VERSION: u32 = 1;

impl SyncState {
    pub fn path(base_dir: &Path) -> PathBuf {
        base_dir.join("state.json")
    }

    /// Load state from disk. Missing or corrupt files yield an empty state.
    pub fn load(base_dir: &Path) -> SyncState {
        let path = Self::path(base_dir);
        match std::fs::read_to_string(&path) {
            Ok(content) => match serde_json::from_str::<SyncState>(&content) {
                Ok(state) if state.version <= STATE_VERSION => state,
                Ok(_) => {
                    crate::log_warn!("state.json has a newer version; starting fresh");
                    SyncState::empty()
                }
                Err(e) => {
                    crate::log_warn!("state.json is corrupt ({}); re-ingesting from scratch", e);
                    SyncState::empty()
                }
            },
            Err(_) => SyncState::empty(),
        }
    }

    fn empty() -> SyncState {
        SyncState {
            version: STATE_VERSION,
            files: BTreeMap::new(),
        }
    }

    /// Atomically persist state (write to tmp, then rename).
    pub fn save(&self, base_dir: &Path) -> Result<()> {
        std::fs::create_dir_all(base_dir)
            .with_context(|| format!("Failed to create base directory: {:?}", base_dir))?;
        let path = Self::path(base_dir);
        let tmp = path.with_extension("json.tmp");
        let content = serde_json::to_string_pretty(self).context("Failed to serialize state")?;
        std::fs::write(&tmp, content)
            .with_context(|| format!("Failed to write state file: {:?}", tmp))?;
        std::fs::rename(&tmp, &path)
            .with_context(|| format!("Failed to replace state file: {:?}", path))?;
        Ok(())
    }

    pub fn get(&self, path: &Path) -> Option<&FileState> {
        self.files.get(&canonical_key(path))
    }

    pub fn insert(&mut self, path: &Path, state: FileState) {
        self.files.insert(canonical_key(path), state);
    }

    /// Reset all cursors to offset 0 (used by index rebuild).
    pub fn reset_offsets(&mut self) {
        for file in self.files.values_mut() {
            file.offset = 0;
            file.size = 0;
            file.mtime_ms = None;
            file.fingerprint = String::new();
        }
    }
}

/// Stable key for a file path: canonicalized when possible, otherwise
/// absolutized against the current directory. Must be applied consistently
/// on both insert and lookup.
pub fn canonical_key(path: &Path) -> String {
    match std::fs::canonicalize(path) {
        Ok(p) => p.to_string_lossy().to_string(),
        Err(_) => {
            let abs = if path.is_absolute() {
                path.to_path_buf()
            } else {
                std::env::current_dir()
                    .map(|cwd| cwd.join(path))
                    .unwrap_or_else(|_| path.to_path_buf())
            };
            abs.to_string_lossy().to_string()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn sample_file_state() -> FileState {
        FileState {
            source: "backend".to_string(),
            offset: 100,
            size: 100,
            mtime_ms: Some(1_752_561_000_000),
            fingerprint: "a1b2c3d4e5f60718".to_string(),
            last_ingested_at: "2026-07-15T10:30:00.123Z".to_string(),
        }
    }

    #[test]
    fn test_load_missing_returns_empty() {
        let dir = TempDir::new().unwrap();
        let state = SyncState::load(dir.path());
        assert_eq!(state.version, STATE_VERSION);
        assert!(state.files.is_empty());
    }

    #[test]
    fn test_load_corrupt_returns_empty() {
        let dir = TempDir::new().unwrap();
        std::fs::write(SyncState::path(dir.path()), "{not json").unwrap();
        let state = SyncState::load(dir.path());
        assert!(state.files.is_empty());
    }

    #[test]
    fn test_save_and_reload_roundtrip() {
        let dir = TempDir::new().unwrap();
        let log = dir.path().join("app.log");
        std::fs::write(&log, "line\n").unwrap();

        let mut state = SyncState::empty();
        state.insert(&log, sample_file_state());
        state.save(dir.path()).unwrap();

        let reloaded = SyncState::load(dir.path());
        let entry = reloaded.get(&log).expect("entry should survive roundtrip");
        assert_eq!(entry.source, "backend");
        assert_eq!(entry.offset, 100);
    }

    #[test]
    fn test_reset_offsets() {
        let dir = TempDir::new().unwrap();
        let log = dir.path().join("app.log");
        std::fs::write(&log, "line\n").unwrap();

        let mut state = SyncState::empty();
        state.insert(&log, sample_file_state());
        state.reset_offsets();

        let entry = state.get(&log).unwrap();
        assert_eq!(entry.offset, 0);
        assert_eq!(entry.size, 0);
        assert!(entry.fingerprint.is_empty());
        // Source assignment survives a reset
        assert_eq!(entry.source, "backend");
    }

    #[test]
    fn test_canonical_key_consistent_for_relative_and_absolute() {
        let dir = TempDir::new().unwrap();
        let log = dir.path().join("app.log");
        std::fs::write(&log, "line\n").unwrap();

        let canonical = std::fs::canonicalize(&log).unwrap();
        assert_eq!(canonical_key(&log), canonical_key(&canonical));
    }
}
