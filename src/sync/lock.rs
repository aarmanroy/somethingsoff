//! Cross-process writer lock at `<base_dir>/.lock`.
//!
//! Exactly one writer (auto-sync, `watch`, `ingest`, `tap`, `index rebuild`)
//! may hold the lock. Read paths must never block: they use `try_acquire` and
//! serve a stale read when the lock is busy. Write commands use
//! `acquire_blocking` with a bounded retry before giving up.

use anyhow::{Context, Result};
use fs2::FileExt;
use std::fs::File;
use std::path::Path;
use std::time::{Duration, Instant};

const RETRY_INTERVAL: Duration = Duration::from_millis(100);

/// An exclusive advisory lock, released on drop.
pub struct SyncLock {
    _file: File,
}

fn open_lock_file(base_dir: &Path) -> Result<File> {
    std::fs::create_dir_all(base_dir)
        .with_context(|| format!("Failed to create base directory: {:?}", base_dir))?;
    let path = base_dir.join(".lock");
    File::options()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&path)
        .with_context(|| format!("Failed to open lock file: {:?}", path))
}

impl SyncLock {
    /// Try to take the lock without waiting. `Ok(None)` means another
    /// process holds it.
    pub fn try_acquire(base_dir: &Path) -> Result<Option<SyncLock>> {
        let file = open_lock_file(base_dir)?;
        match file.try_lock_exclusive() {
            Ok(()) => Ok(Some(SyncLock { _file: file })),
            Err(_) => Ok(None),
        }
    }

    /// Take the lock, retrying for up to `timeout` before failing.
    pub fn acquire_blocking(base_dir: &Path, timeout: Duration) -> Result<SyncLock> {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(lock) = Self::try_acquire(base_dir)? {
                return Ok(lock);
            }
            if Instant::now() >= deadline {
                anyhow::bail!(
                    "Another somethingsoff process holds the index lock (a `watch`, `tap`, or ingest is running)"
                );
            }
            std::thread::sleep(RETRY_INTERVAL);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_acquire_and_release() {
        let dir = TempDir::new().unwrap();
        let lock = SyncLock::try_acquire(dir.path()).unwrap();
        assert!(lock.is_some());
        drop(lock);
        // Released on drop: can be re-acquired.
        assert!(SyncLock::try_acquire(dir.path()).unwrap().is_some());
    }

    #[test]
    fn test_blocking_acquire_times_out_quickly_when_free() {
        let dir = TempDir::new().unwrap();
        let start = Instant::now();
        let _lock = SyncLock::acquire_blocking(dir.path(), Duration::from_secs(5)).unwrap();
        assert!(start.elapsed() < Duration::from_secs(1));
    }
}
