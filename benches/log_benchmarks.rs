use criterion::{black_box, criterion_group, criterion_main, BenchmarkId, Criterion};
use somethingsoff::cmd::errors::{generate_fingerprint, ErrorsCommand};
use somethingsoff::config::Config;
use somethingsoff::index::builder::IndexBuilder;
use somethingsoff::index::searcher::{IndexSearcher, SearchOptions};
use somethingsoff::index::upsert::{count_docs, upsert_entry};
use somethingsoff::schema::{create_schema, ErrorInfo, LogEntry, LogFields};
use std::fs::File;
use std::io::Write;
use tantivy::directory::RamDirectory;
use tantivy::Index;
use tempfile::TempDir;

fn bench_fingerprint(c: &mut Criterion) {
    let name = "DatabaseError";
    let message = "Failed to connect to the database after 3 retries. Connection timeout occurred at 2026-03-22T10:00:00Z.";

    c.bench_function("generate_fingerprint", |b| {
        b.iter(|| generate_fingerprint(black_box(name), black_box(message)))
    });
}

fn bench_json_serialization(c: &mut Criterion) {
    let entry = LogEntry {
        log_id: "a1b2c3d4e5f67890".to_string(),
        timestamp: "2026-03-22T10:00:00.123Z".to_string(),
        level: "error".to_string(),
        source: "backend".to_string(),
        message: "Failed to process request".to_string(),
        request_id: Some("req-123".to_string()),
        user_id: Some("user-456".to_string()),
        route: Some("/api/v1/resource".to_string()),
        method: Some("POST".to_string()),
        status_code: Some(500),
        duration_ms: Some(150.5),
        error: Some(ErrorInfo {
            name: Some("InternalError".to_string()),
            message: Some("Something went wrong".to_string()),
            code: Some("E500".to_string()),
        }),
        source_file: Some("src/index.ts".to_string()),
        line_number: Some(10),
        attributes: None,
        parse_format: "json".to_string(),
    };

    c.bench_function("log_entry_to_json_sorted", |b| {
        b.iter(|| entry.to_json_sorted().unwrap())
    });
}

fn bench_ingestion(c: &mut Criterion) {
    let temp_dir = TempDir::new().unwrap();
    let log_path = temp_dir.path().join("test.log");
    let mut file = File::create(&log_path).unwrap();

    // Create 1000 log lines for ingestion
    for i in 0..1000 {
        writeln!(file, r#"{{"timestamp":"2026-03-22T10:00:{:02}.{:03}Z","level":"info","source":"bench","message":"Log message {}","request_id":"req-{}"}}"#, (i / 60) % 60, i % 60, i, i).unwrap();
    }

    let mut config = Config::default();
    config.general.index_path = temp_dir.path().join("index");
    config
        .log_sources
        .insert("bench".to_string(), log_path.to_str().unwrap().to_string());

    let builder = IndexBuilder::new(config.clone());

    c.bench_function("ingest_1000_lines", |b| {
        b.iter(|| {
            let index_dir = config.index_dir();
            if index_dir.exists() {
                std::fs::remove_dir_all(index_dir).unwrap();
            }
            builder.build().unwrap()
        })
    });
}

fn bench_search_and_aggregation(c: &mut Criterion) {
    let temp_dir = TempDir::new().unwrap();
    let index_dir = temp_dir.path().join("index");
    std::fs::create_dir_all(&index_dir).unwrap();

    let log_path = temp_dir.path().join("test.log");
    let mut file = File::create(&log_path).unwrap();

    // Create 5000 log lines to have a decent index size
    for i in 0..5000 {
        let level = if i % 10 == 0 { "error" } else { "info" };
        let user_id = format!("user-{}", i % 50);
        writeln!(file, r#"{{"timestamp":"2026-03-22T10:00:{:02}.{:03}Z","level":"{}","source":"bench","message":"Log message {} with search_term","request_id":"req-{}","user_id":"{}","error":{{"name":"Error{}","message":"Something went wrong"}} }}"#,
            (i / 100) % 60, i % 100, level, i, i, user_id, i % 5).unwrap();
    }

    let mut config = Config::default();
    config.general.index_path = index_dir.clone();
    config
        .log_sources
        .insert("bench".to_string(), log_path.to_str().unwrap().to_string());

    // Build the index once
    let builder = IndexBuilder::new(config.clone());
    builder.build().unwrap();

    let searcher = IndexSearcher::new(config.clone()).unwrap();

    c.bench_function("search_single_term", |b| {
        let options = SearchOptions {
            query: Some("search_term".to_string()),
            limit: 100,
            ..Default::default()
        };
        b.iter(|| searcher.search(black_box(options.clone())).unwrap())
    });

    c.bench_function("search_with_filter", |b| {
        let options = SearchOptions {
            query: Some("message".to_string()),
            level: Some("error".to_string()),
            limit: 100,
            ..Default::default()
        };
        b.iter(|| searcher.search(black_box(options.clone())).unwrap())
    });

    // Error aggregation benchmark
    std::env::set_var("SOMETHINGSOFF_BASE_DIR", temp_dir.path().to_str().unwrap());
    let mut config_file = File::create(temp_dir.path().join("config.toml")).unwrap();
    writeln!(config_file, "[general]").unwrap();
    writeln!(config_file, "index_path = {:?}", index_dir).unwrap();
    writeln!(config_file, "[log_sources]").unwrap();
    writeln!(config_file, "bench = {:?}", log_path).unwrap();

    let errors_cmd = ErrorsCommand {
        last: "24h".to_string(),
        limit: 10,
    };

    let rt = tokio::runtime::Runtime::new().unwrap();
    c.bench_function("error_aggregation_500_errors", |b| {
        b.to_async(&rt)
            .iter(|| async { errors_cmd.execute().await.unwrap() })
    });
}

// -------------------------------------------------------------------------
// NEW: Upsert overhead benchmarks
// -------------------------------------------------------------------------

/// Generate N unique log entries for benchmarking
fn make_entries(n: usize) -> Vec<LogEntry> {
    (0..n)
        .map(|i| LogEntry {
            log_id: format!("{i:016x}"),
            timestamp: format!("2026-03-22T10:00:{:02}.{:03}Z", (i / 60) % 60, i % 60),
            level: if i % 10 == 0 { "error" } else { "info" }.to_string(),
            source: "bench".to_string(),
            message: format!("Benchmark message {i}"),
            request_id: Some(format!("req-{i}")),
            user_id: if i % 3 == 0 {
                Some(format!("user-{i}"))
            } else {
                None
            },
            route: if i % 5 == 0 {
                Some("/api/test".to_string())
            } else {
                None
            },
            method: None,
            status_code: if i % 10 == 0 { Some(500) } else { None },
            duration_ms: if i % 2 == 0 {
                Some(i as f64 * 1.5)
            } else {
                None
            },
            error: if i % 10 == 0 {
                Some(ErrorInfo {
                    name: Some("BenchError".to_string()),
                    message: Some("bench error".to_string()),
                    code: None,
                })
            } else {
                None
            },
            source_file: None,
            line_number: None,
            attributes: None,
            parse_format: "json".to_string(),
        })
        .collect()
}

fn bench_upsert_overhead(c: &mut Criterion) {
    let mut group = c.benchmark_group("upsert_overhead");

    for size in [100, 1_000, 10_000].iter() {
        let entries = make_entries(*size);

        // Benchmark 1: insert WITHOUT upsert (skip_dedup = true)
        // This measures baseline document insertion performance
        group.bench_with_input(BenchmarkId::new("insert_no_dedup", size), size, |b, &_| {
            b.iter(|| {
                let schema = create_schema();
                let fields = LogFields::new(&schema).unwrap();
                let dir = RamDirectory::create();
                let index = Index::open_or_create(dir, schema).unwrap();
                let mut writer = index.writer_with_num_threads(1, 15_000_000).unwrap();

                for entry in &entries {
                    upsert_entry(&mut writer, &fields, entry, true).unwrap();
                }
                writer.commit().unwrap();
            })
        });

        // Benchmark 2: insert WITH upsert (skip_dedup = false)
        // This measures the overhead of delete_term on an empty index
        group.bench_with_input(
            BenchmarkId::new("insert_with_dedup_empty", size),
            size,
            |b, &_| {
                b.iter(|| {
                    let schema = create_schema();
                    let fields = LogFields::new(&schema).unwrap();
                    let dir = RamDirectory::create();
                    let index = Index::open_or_create(dir, schema).unwrap();
                    let mut writer = index.writer_with_num_threads(1, 15_000_000).unwrap();

                    for entry in &entries {
                        upsert_entry(&mut writer, &fields, entry, false).unwrap();
                    }
                    writer.commit().unwrap();
                })
            },
        );

        // Benchmark 3: upsert into an EXISTING index (re-ingest same data)
        // This measures the cost of delete_term when docs actually exist
        group.bench_with_input(
            BenchmarkId::new("reingest_with_dedup", size),
            size,
            |b, &_| {
                b.iter(|| {
                    let schema = create_schema();
                    let fields = LogFields::new(&schema).unwrap();
                    let dir = RamDirectory::create();
                    let index = Index::open_or_create(dir, schema).unwrap();
                    let mut writer = index.writer_with_num_threads(1, 15_000_000).unwrap();

                    // First pass: insert all entries
                    for entry in &entries {
                        upsert_entry(&mut writer, &fields, entry, true).unwrap();
                    }
                    writer.commit().unwrap();

                    // Second pass: re-ingest with upsert (the real-world scenario)
                    for entry in &entries {
                        upsert_entry(&mut writer, &fields, entry, false).unwrap();
                    }
                    writer.commit().unwrap();

                    let reader = index.reader().unwrap();
                    let count = count_docs(&reader);
                    assert_eq!(
                        count, *size as u64,
                        "Should have exactly {size} unique docs"
                    );
                })
            },
        );
    }

    group.finish();
}

criterion_group!(
    benches,
    bench_fingerprint,
    bench_json_serialization,
    bench_ingestion,
    bench_search_and_aggregation,
    bench_upsert_overhead
);
criterion_main!(benches);
