# AGENTS.md — codebase guide for AI agents

This file explains what `somethingsoff` is and how the codebase fits together,
so an agent can navigate and modify it without re-deriving the design.

## What this program is

A **local-first log search CLI** for developers and AI coding agents. Apps
write log files (or pipe stdout through `tap`); this tool indexes them with
[Tantivy](https://github.com/quickwit-oss/tantivy) (a Rust Lucene) and
answers search/aggregation queries in milliseconds. The defining property is
**auto-ingest-on-query**: read commands transparently discover sources,
create the index on demand, and ingest only new bytes before answering — so
there is *no setup step, no daemon, and no staleness*. Nothing leaves the
machine; PII is redacted at ingest.

Two audiences:
1. Developers debugging locally (`somethingsoff errors --last 1h`).
2. Coding agents (Claude Code etc.) driving the CLI via Bash. Every command
   emits one stable JSON envelope with machine-readable error codes and
   recovery `hint`s — see `docs/CONTRACT.md`, the frozen v1 spec that both
   the shipped Agent Skill (`claude-plugin/skills/somethingsoff/SKILL.md`) and `tests/contract.rs`
   assert against.

**Positioning:** competitors are either stateless local transformers
(Kelora, lnav — re-read files per invocation, no memory of "is this error
new?") or cloud/backend observability platforms (Sentry Seer, Datadog —
setup, auth, telemetry leaves the machine). This tool owns the intersection:
capture → persistent index → agent contract, with zero setup.

## Architecture map

```
src/
  main.rs            clap CLI: global flags (--quiet/--no-redact/--no-sync/
                     --format), command dispatch, error-envelope catch-all
  lib.rs             process globals (QUIET/NO_REDACT/NO_SYNC/JSONL) + log macros
  config.rs          base_dir() = $SOMETHINGSOFF_BASE_DIR or ./.somethingsoff;
                     config.toml (all optional); [sync] knobs
  output.rs          THE CONTRACT: Envelope builder, ErrorCode→exit-code map,
                     CliError{code,message,hint}, classify_error()
  error.rs           LogServiceError (legacy typed errors) → ErrorCode mapping
  schema.rs          LogEntry (15 fields incl. `attributes`), RawLogEntry +
                     from_value() alias mapping (camelCase/epoch/msg→message),
                     normalize_timestamp(), log_id content hash, SCHEMA_VERSION,
                     Tantivy schema (create_schema)
  pii.rs             regex redaction (emails/tokens/cards/secrets), applied
                     at ingest incl. recursive attribute values
  sync/              THE ENGINE (heart of "just works")
    mod.rs           sync_before_read(): fast path (stat vs state) → lock →
                     run_sync(); needs_migration()/migrate_index() for
                     SCHEMA_VERSION bumps (transparent rebuild)
    discover.rs      source union: config [log_sources] + ./logs/*.{log,json,
                     jsonl} + tap journals (streams/*.jsonl) + state entries
    tail.rs          FileCursor, rotation detection (shrink or head-
                     fingerprint change), ingest_new_lines() — the ONE
                     read→parse→redact→upsert loop
    state.rs         state.json per-file cursors (atomic write; disposable:
                     dedup makes re-ingest-from-0 correct)
    lock.rs          fs2 writer lock at .somethingsoff/.lock; readers try_lock
                     and degrade to stale read, writers retry ≤5s
  parser/
    detector.rs      per-line format detection, priority: JSON → logfmt →
                     apache → python → go-logrus → log4j → syslog
    parsers/         one parser per format; logfmt/JSON route through
                     RawLogEntry::from_value so aliases + attributes apply
    learn.rs         regex suggestions for unknown formats (advisory)
  index/
    builder.rs       full rebuild = reset cursors + one sync pass
    searcher.rs      IndexSearcher: build_query (term/range filters, --last
                     lower-bound-only), search/get, bounded --context via
                     two range queries, sample_entries() for schema profiling
    aggregator.rs    streaming collectors: stats counts; error groups keyed
                     by (error_name, normalize_template(msg)) — "drain-lite"
    upsert.rs        delete_term(log_id)+add; builds full_text incl.
                     "key value" tokens for scalar attributes
  cmd/               one file per command; read commands go through
                     cmd::prepare_read() (sync + searcher); all output via
                     output::Envelope. tap.rs journals stdin then indexes
                     inline when the lock is free. claude.rs installs the
                     embedded SKILL.md; claude_hook.rs is the plugin's
                     PostToolUseFailure hook (stdin: event JSON, stdout:
                     additionalContext or nothing, always exit 0 —
                     envelope-exempt, see CONTRACT.md).
claude-plugin/       the Claude Code plugin: .claude-plugin/plugin.json,
                     hooks/hooks.json, skills/somethingsoff/SKILL.md (the
                     single source of the skill — claude.rs include_str!s
                     this exact file). Repo root's .claude-plugin/
                     marketplace.json makes the repo itself a marketplace.
docs/CONTRACT.md     frozen v1 output contract (envelope/exit codes/behavior)
tests/               integration suites: contract, autosync, attributes, tap,
                     claude_install, claude_hook, + legacy (ingest_search, …)
```

## Data flow

**Read path** (`search`/`get`/`stats`/`errors`/`schema`/`health`):
`Config::load` → `sync_before_read` (fast path: one state.json read + one
stat per source; else try-lock + tail new bytes + commit + save state) →
`IndexSearcher` query → `Envelope::emit` (stdout, sorted keys).

**Write paths** (hold the lock for their lifetime): `watch` (poll loop,
re-discovers sources each tick), `tap` (stdin → passthrough + journal +
inline upsert, commit per 500 entries/1s), `ingest` (one-shot full file,
registers cursor so auto-sync takes over).

## Invariants (do not break)

1. **stdout is exactly one JSON document** (or jsonl records). Diagnostics
   only on stderr. `tap` is the sole exception (summary → stderr).
2. **One writer at a time** via `.somethingsoff/.lock`; **readers never block
   and never prompt** — degrade to stale read + `sync_deferred` notice.
3. **State is disposable**: `log_id = sha256(timestamp|level|source|message|
   request_id)[:16]` dedup means lost/corrupt state.json or double ingestion
   can never produce duplicates. Any change to log_id inputs or the Tantivy
   schema REQUIRES bumping `SCHEMA_VERSION` (schema.rs) — migration then
   happens transparently.
4. **No input field is silently dropped** — unmapped JSON keys go to
   `attributes` (stored + full-text searchable).
5. **Deterministic output**: sorted JSON keys, all 15 entry fields present
   (null when absent) unless `--compact`/`--fields`.
6. `docs/CONTRACT.md` is load-bearing: changing envelope shapes, exit codes,
   or error codes is a breaking change; update the doc, `claude-plugin/skills/somethingsoff/SKILL.md`,
   and `tests/contract.rs` together.

## Developing

```bash
cargo test                                   # 14 suites, all must pass
cargo clippy --all-targets -- -D warnings    # CI-enforced
cargo fmt --all                              # CI-enforced
```

Manual smoke: `mkdir -p /tmp/d/logs && cd /tmp/d && echo '{"level":"error","message":"x"}' > logs/a.log && somethingsoff search --level error`.

Known gotchas:
- Tests set `SOMETHINGSOFF_BASE_DIR` to temp dirs; never rely on cwd state.
- A file's final line without trailing newline is ingested immediately; if a
  writer later appends to that same line, fragments result (rare; `tap`
  avoids it). Documented limitation.
- `--last` deliberately has no upper time bound (clock-skew tolerance).
- RFC3164 syslog has no year → current year is assumed.
