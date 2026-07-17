//! Schema command - discover what's actually in the indexed logs.
//!
//! Instead of a hardcoded field list, this profiles the newest entries
//! (up to 10k) and reports, per field — including dynamic attribute keys —
//! the observed type, how many entries carry it, its (capped) cardinality,
//! and a few sample values. This is the command an agent runs first to
//! learn what it can filter on.

use anyhow::Result;
use chrono::Utc;
use clap::Args;
use serde::Serialize;
use std::collections::{BTreeMap, HashMap, HashSet};

use crate::config::Config;
use crate::output::Envelope;
use crate::schema::LogEntry;

/// Cap for the profiling scan (newest N entries).
const PROFILE_SAMPLE_LIMIT: usize = 10_000;
/// Cardinality counting stops at this many distinct values.
const CARDINALITY_CAP: usize = 1_000;
/// Distinct sample values reported per field.
const SAMPLES_PER_FIELD: usize = 3;

/// Schema command for discovery
#[derive(Args)]
pub struct SchemaCommand {}

#[derive(Debug, Serialize)]
pub struct SystemInfo {
    pub current_time_utc: String,
    pub timezone: String,
}

#[derive(Debug, Serialize)]
pub struct IndexStats {
    pub total_entries: usize,
    pub oldest_entry: String,
    pub newest_entry: String,
    pub sources: Vec<String>,
    pub levels_count: HashMap<String, usize>,
}

/// Observed profile of one field across the sampled entries.
#[derive(Debug, Serialize)]
pub struct FieldProfile {
    pub name: String,
    pub r#type: String,
    /// Entries (of the sample) carrying a non-null value
    pub count: usize,
    /// Distinct values seen, as a string ("1000+" when capped)
    pub cardinality: String,
    /// Up to 3 distinct sample values
    pub samples: Vec<String>,
}

/// Accumulator used while folding entries into field profiles.
#[derive(Default)]
struct FieldAcc {
    r#type: &'static str,
    count: usize,
    values: HashSet<String>,
    capped: bool,
    samples: Vec<String>,
}

impl FieldAcc {
    fn observe(&mut self, r#type: &'static str, value: String) {
        self.r#type = r#type;
        self.count += 1;
        if self.capped {
            return;
        }
        if self.values.insert(value.clone()) {
            if self.samples.len() < SAMPLES_PER_FIELD {
                self.samples.push(value);
            }
            if self.values.len() >= CARDINALITY_CAP {
                self.capped = true;
                self.values.clear(); // free memory; we only report "1000+"
            }
        }
    }
}

fn profile_fields(entries: &[LogEntry]) -> Vec<FieldProfile> {
    let mut accs: BTreeMap<String, FieldAcc> = BTreeMap::new();
    let observe = |accs: &mut BTreeMap<String, FieldAcc>,
                   name: &str,
                   r#type: &'static str,
                   value: Option<String>| {
        if let Some(value) = value {
            accs.entry(name.to_string())
                .or_default()
                .observe(r#type, value);
        }
    };

    for entry in entries {
        observe(
            &mut accs,
            "timestamp",
            "string",
            Some(entry.timestamp.clone()),
        );
        observe(&mut accs, "level", "string", Some(entry.level.clone()));
        observe(&mut accs, "source", "string", Some(entry.source.clone()));
        observe(
            &mut accs,
            "parse_format",
            "string",
            Some(entry.parse_format.clone()),
        );
        observe(&mut accs, "message", "text", Some(entry.message.clone()));
        observe(&mut accs, "request_id", "string", entry.request_id.clone());
        observe(&mut accs, "user_id", "string", entry.user_id.clone());
        observe(&mut accs, "route", "string", entry.route.clone());
        observe(&mut accs, "method", "string", entry.method.clone());
        observe(
            &mut accs,
            "status_code",
            "integer",
            entry.status_code.map(|v| v.to_string()),
        );
        observe(
            &mut accs,
            "duration_ms",
            "float",
            entry.duration_ms.map(|v| v.to_string()),
        );
        if let Some(ref error) = entry.error {
            observe(&mut accs, "error.name", "string", error.name.clone());
            observe(&mut accs, "error.message", "text", error.message.clone());
        }
        observe(
            &mut accs,
            "source_file",
            "string",
            entry.source_file.clone(),
        );
        observe(
            &mut accs,
            "line_number",
            "integer",
            entry.line_number.map(|v| v.to_string()),
        );
        if let Some(ref attributes) = entry.attributes {
            for (key, value) in attributes {
                let (r#type, rendered): (&'static str, String) = match value {
                    serde_json::Value::String(s) => ("string", s.clone()),
                    serde_json::Value::Number(n) => ("number", n.to_string()),
                    serde_json::Value::Bool(b) => ("boolean", b.to_string()),
                    other => ("object", other.to_string()),
                };
                observe(
                    &mut accs,
                    &format!("attributes.{}", key),
                    r#type,
                    Some(rendered),
                );
            }
        }
    }

    accs.into_iter()
        .map(|(name, acc)| FieldProfile {
            name,
            r#type: acc.r#type.to_string(),
            count: acc.count,
            cardinality: if acc.capped {
                format!("{}+", CARDINALITY_CAP)
            } else {
                acc.values.len().to_string()
            },
            samples: acc.samples,
        })
        .collect()
}

impl SchemaCommand {
    pub async fn execute(&self) -> Result<u8> {
        let mut envelope = Envelope::new("schema");
        let config = Config::load()?;
        let index_path = config.index_dir().to_string_lossy().to_string();

        let system_info = SystemInfo {
            current_time_utc: Utc::now().to_rfc3339(),
            timezone: "UTC".to_string(),
        };

        let mut index_stats = IndexStats {
            total_entries: 0,
            oldest_entry: "".into(),
            newest_entry: "".into(),
            sources: Vec::new(),
            levels_count: HashMap::new(),
        };
        let mut fields: Vec<FieldProfile> = Vec::new();
        let mut sampled: usize = 0;

        // Sync then profile actual data (index is created on demand)
        if let Ok((searcher, sync_report)) = crate::cmd::prepare_read(config) {
            envelope.set_sync(sync_report);
            if let Ok(stats) = searcher.get_index_stats() {
                index_stats = stats;
            }
            if let Ok(entries) = searcher.sample_entries(PROFILE_SAMPLE_LIMIT) {
                sampled = entries.len();
                fields = profile_fields(&entries);
            }
        }

        if index_stats.total_entries > sampled {
            envelope.notice(
                "sampled",
                &format!(
                    "Field profile computed from the newest {} of {} entries",
                    sampled, index_stats.total_entries
                ),
                None,
            );
        }

        envelope.emit(
            serde_json::json!({
                "index_path": index_path,
                "system_info": system_info,
                "schema": {
                    "version": crate::schema::SCHEMA_VERSION.to_string(),
                    "fields": fields,
                    "index_stats": index_stats,
                    "supported_time_formats": [
                        "1h", "24h", "7d", "30d", "1w", "ISO8601"
                    ],
                },
            }),
            None,
        )?;

        Ok(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(level: &str, attrs: Option<Vec<(&str, serde_json::Value)>>) -> LogEntry {
        LogEntry {
            log_id: "x".into(),
            timestamp: "2026-07-15T10:00:00.000Z".into(),
            level: level.into(),
            source: "app".into(),
            message: "hello".into(),
            request_id: None,
            user_id: None,
            route: None,
            method: None,
            status_code: Some(200),
            duration_ms: None,
            error: None,
            source_file: None,
            line_number: None,
            attributes: attrs.map(|kvs| kvs.into_iter().map(|(k, v)| (k.to_string(), v)).collect()),
            parse_format: "raw".to_string(),
        }
    }

    #[test]
    fn test_profile_includes_attribute_keys() {
        let entries = vec![
            entry(
                "info",
                Some(vec![("service", serde_json::json!("checkout"))]),
            ),
            entry("error", Some(vec![("service", serde_json::json!("auth"))])),
        ];
        let profiles = profile_fields(&entries);

        let service = profiles
            .iter()
            .find(|p| p.name == "attributes.service")
            .expect("attribute key profiled");
        assert_eq!(service.count, 2);
        assert_eq!(service.cardinality, "2");
        assert_eq!(service.r#type, "string");

        let level = profiles.iter().find(|p| p.name == "level").unwrap();
        assert_eq!(level.count, 2);
        assert_eq!(level.cardinality, "2");

        // Absent fields don't produce profiles
        assert!(profiles.iter().all(|p| p.name != "user_id"));
    }

    #[test]
    fn test_profile_samples_are_distinct_and_capped() {
        let entries: Vec<LogEntry> = (0..10)
            .map(|i| {
                entry(
                    "info",
                    Some(vec![("req", serde_json::json!(format!("id-{}", i)))]),
                )
            })
            .collect();
        let profiles = profile_fields(&entries);
        let req = profiles
            .iter()
            .find(|p| p.name == "attributes.req")
            .unwrap();
        assert_eq!(req.count, 10);
        assert_eq!(req.cardinality, "10");
        assert_eq!(req.samples.len(), SAMPLES_PER_FIELD);
    }
}
