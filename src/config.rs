//! Configuration management for somethingsoff

use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Main configuration structure.
///
/// Note: zero-config source discovery (the ./logs scan) happens at sync
/// time in `sync::discover`, not here, so it runs on every invocation and
/// also applies when a config.toml exists.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub general: GeneralConfig,

    #[serde(default)]
    pub log_sources: HashMap<String, String>,

    #[serde(default)]
    pub output: OutputConfig,

    #[serde(default)]
    pub sync: SyncConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GeneralConfig {
    /// Where to store the search index
    #[serde(default = "default_index_path")]
    pub index_path: PathBuf,

    /// How long to keep logs in index (days)
    #[serde(default = "default_retention_days")]
    pub retention_days: u32,
}

impl Default for GeneralConfig {
    fn default() -> Self {
        Self {
            index_path: default_index_path(),
            retention_days: default_retention_days(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OutputConfig {
    /// Always include these fields in output
    #[serde(default = "default_fields")]
    pub default_fields: Vec<String>,
}

impl Default for OutputConfig {
    fn default() -> Self {
        Self {
            default_fields: default_fields(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SyncConfig {
    /// Automatically ingest new log data before answering read commands
    #[serde(default = "default_sync_auto")]
    pub auto: bool,

    /// Polling interval for `watch` mode (seconds)
    #[serde(default = "default_poll_interval")]
    pub poll_interval_secs: u64,
}

impl Default for SyncConfig {
    fn default() -> Self {
        Self {
            auto: default_sync_auto(),
            poll_interval_secs: default_poll_interval(),
        }
    }
}

fn default_sync_auto() -> bool {
    true
}
fn default_poll_interval() -> u64 {
    2
}

/// Base directory for all per-project state (config, index, sync state,
/// tap journals). Always project-local (`./.somethingsoff`, created lazily)
/// unless overridden via `SOMETHINGSOFF_BASE_DIR`.
pub fn base_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("SOMETHINGSOFF_BASE_DIR") {
        return PathBuf::from(dir);
    }
    PathBuf::from(".somethingsoff")
}

/// Create `base_dir()` and make it exclude itself from git.
///
/// Everything under this directory is derived from log content — the index,
/// and the `tap` journals most of all — which is exactly the material most
/// likely to carry a credential. Writing the `.gitignore` at creation time
/// means the directory cannot be committed by accident without the user
/// having to know it exists. Idempotent, and never overwrites a file the
/// user has edited.
pub fn ensure_base_dir() -> std::io::Result<PathBuf> {
    let dir = base_dir();
    std::fs::create_dir_all(&dir)?;
    let ignore = dir.join(".gitignore");
    if !ignore.exists() {
        std::fs::write(
            &ignore,
            "# Created by somethingsoff. Holds indexed log content; never commit it.\n*\n",
        )?;
    }
    Ok(dir)
}

fn default_index_path() -> PathBuf {
    base_dir().join("index")
}

fn default_retention_days() -> u32 {
    30
}
fn default_fields() -> Vec<String> {
    vec![
        "timestamp".into(),
        "level".into(),
        "message".into(),
        "request_id".into(),
    ]
}

impl Config {
    /// Load configuration from file or return defaults
    pub fn load() -> Result<Self> {
        let config_path = Self::config_path();

        if config_path.exists() {
            let content = std::fs::read_to_string(&config_path).map_err(|e| {
                crate::error::LogServiceError::Config(format!("Failed to read config file: {}", e))
            })?;
            let config: Config = toml::from_str(&content).map_err(|e| {
                crate::error::LogServiceError::Config(format!("Failed to parse config file: {}", e))
            })?;
            Ok(config)
        } else {
            Ok(Self::default())
        }
    }

    /// Get the configuration file path
    pub fn config_path() -> PathBuf {
        base_dir().join("config.toml")
    }

    /// Get the index directory path
    pub fn index_dir(&self) -> &Path {
        &self.general.index_path
    }

    /// Get all log file paths
    #[allow(dead_code)]
    pub fn log_files(&self) -> Vec<PathBuf> {
        self.log_sources.values().map(PathBuf::from).collect()
    }
}
