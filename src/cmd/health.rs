//! Health command - system and index health check
//!
//! Reports real figures (document counts from the index reader, newest
//! entry timestamp, source freshness, lock status) in the v1 envelope.
//! Exit code 0 when healthy/degraded, 1 when unhealthy.

use anyhow::Result;
use clap::Args;
use serde::Serialize;
use std::path::Path;

use crate::config::Config;
use crate::output::Envelope;
use crate::sync::state::SyncState;

/// Check system and index health
#[derive(Args)]
pub struct HealthCommand {}

#[derive(Debug, Serialize)]
struct Check {
    name: String,
    status: String,
    details: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    data: Option<serde_json::Value>,
}

impl HealthCommand {
    pub fn execute(&self) -> Result<u8> {
        let mut envelope = Envelope::new("health");
        let config = Config::load()?;
        // Sync first so the report reflects current log data.
        let sync_report = crate::sync::sync_before_read(&config)?;
        envelope.set_sync(sync_report);

        let checks = vec![
            check_index(&config),
            check_sources(&config),
            check_disk(&config),
            check_lock(),
        ];

        let unhealthy = checks.iter().any(|c| c.status == "unhealthy");
        let degraded = checks.iter().any(|c| c.status == "degraded");
        let status = if unhealthy {
            "unhealthy"
        } else if degraded {
            "degraded"
        } else {
            "healthy"
        };

        envelope.emit(
            serde_json::json!({ "status": status, "checks": checks }),
            None,
        )?;

        Ok(if unhealthy { 1 } else { 0 })
    }
}

fn check_index(config: &Config) -> Check {
    match crate::sync::open_or_create_index(config) {
        Ok((index, _fields)) => match index.reader() {
            Ok(reader) => {
                let searcher = reader.searcher();
                let docs = searcher.num_docs();
                let size = get_dir_size(config.index_dir());
                let size_mb = (size as f64 / (1024.0 * 1024.0) * 100.0).round() / 100.0;
                Check {
                    name: "index".to_string(),
                    status: "healthy".to_string(),
                    details: format!("{} documents indexed", docs),
                    data: Some(serde_json::json!({
                        "documents": docs,
                        "size_mb": size_mb,
                        "path": config.index_dir().display().to_string(),
                    })),
                }
            }
            Err(e) => Check {
                name: "index".to_string(),
                status: "unhealthy".to_string(),
                details: format!("Index unreadable: {}", e),
                data: None,
            },
        },
        Err(e) => Check {
            name: "index".to_string(),
            status: "unhealthy".to_string(),
            details: format!("Cannot open index: {}", e),
            data: None,
        },
    }
}

fn check_sources(config: &Config) -> Check {
    let base_dir = crate::config::base_dir();
    let state = SyncState::load(&base_dir);
    let sources = crate::sync::discover::discover_sources(config, &state);

    if sources.is_empty() {
        return Check {
            name: "sources".to_string(),
            status: "degraded".to_string(),
            details: "No log sources found".to_string(),
            data: Some(serde_json::json!({
                "hint": "Put log files in ./logs/, or add sources to .somethingsoff/config.toml",
                "count": 0,
            })),
        };
    }

    let missing: Vec<String> = sources
        .iter()
        .filter(|(_, path)| !path.exists())
        .map(|(name, path)| format!("{} ({})", name, path.display()))
        .collect();

    let readable = sources.len() - missing.len();
    let status = if readable == 0 { "degraded" } else { "healthy" };
    Check {
        name: "sources".to_string(),
        status: status.to_string(),
        details: format!("{} of {} sources readable", readable, sources.len()),
        data: Some(serde_json::json!({
            "count": sources.len(),
            "missing": missing,
        })),
    }
}

fn check_disk(config: &Config) -> Check {
    // Index dir may not exist yet on a truly fresh project; check its parent.
    let path = if config.index_dir().exists() {
        config.index_dir().to_path_buf()
    } else {
        std::path::PathBuf::from(".")
    };

    match fs2::available_space(&path) {
        Ok(available) => {
            let available_mb = available / (1024 * 1024);
            let status = if available_mb < 500 {
                "unhealthy"
            } else if available_mb < 1024 {
                "degraded"
            } else {
                "healthy"
            };

            Check {
                name: "disk".to_string(),
                status: status.to_string(),
                details: format!("{} MB available", available_mb),
                data: Some(serde_json::json!({ "available_mb": available_mb })),
            }
        }
        Err(e) => Check {
            name: "disk".to_string(),
            status: "unhealthy".to_string(),
            details: format!("Failed to check disk: {}", e),
            data: None,
        },
    }
}

fn check_lock() -> Check {
    let base_dir = crate::config::base_dir();
    match crate::sync::lock::SyncLock::try_acquire(&base_dir) {
        Ok(Some(_lock)) => Check {
            name: "lock".to_string(),
            status: "healthy".to_string(),
            details: "Writer lock available".to_string(),
            data: None,
        },
        Ok(None) => Check {
            name: "lock".to_string(),
            status: "healthy".to_string(),
            details: "Writer lock held by another process (watch/tap/ingest running)".to_string(),
            data: Some(serde_json::json!({ "held": true })),
        },
        Err(e) => Check {
            name: "lock".to_string(),
            status: "degraded".to_string(),
            details: format!("Cannot check lock: {}", e),
            data: None,
        },
    }
}

fn get_dir_size(path: &Path) -> u64 {
    let mut size = 0;
    if path.is_dir() {
        if let Ok(entries) = std::fs::read_dir(path) {
            for entry in entries.flatten() {
                if let Ok(metadata) = entry.metadata() {
                    if metadata.is_file() {
                        size += metadata.len();
                    } else if metadata.is_dir() {
                        size += get_dir_size(&entry.path());
                    }
                }
            }
        }
    }
    size
}
