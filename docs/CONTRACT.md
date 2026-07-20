# somethingsoff output contract (v1)

This is the frozen machine contract. The SKILL.md shipped to agents and the
`tests/contract.rs` suite both assert against this document. Changes here are
breaking changes and require a major version bump.

## Channels

- **stdout**: exactly one JSON document per invocation (or one record per
  line with `--format jsonl`). Nothing else, ever.
- **stderr**: human diagnostics (`[INFO]`/`[WARN]`/`[ERROR]` lines).
  Silenced by `--quiet`. Exception: `tap` prints its summary envelope to
  stderr because stdout is reserved for the passthrough stream.
- Exemption: `claude hook` is Claude Code hook plumbing, not an agent-facing
  command — its stdout is a Claude Code hook response (or nothing) and it
  always exits `0`. It never prints the envelope below.
- All JSON output has **sorted keys** (deterministic byte-for-byte for
  identical inputs).

## Success envelope

```json
{
  "ok": true,
  "command": "search",
  "version": "0.1.0",
  "generated_at": "2026-07-15T10:30:00.123Z",
  "elapsed_ms": 12.34,
  "sync": {
    "skipped": false,
    "reason": null,
    "files_checked": 3,
    "ingested": 42,
    "failed": 0,
    "elapsed_ms": 3.1,
    "migrated": true
  },
  "data": {},
  "meta": {},
  "notices": [ { "code": "…", "message": "…", "hint": "…" } ]
}
```

- `sync` present only on read commands (search/get/stats/errors/patterns/schema/health).
  `reason` ∈ `"fresh" | "locked" | "disabled"` when `skipped` is true.
  `migrated` appears (true) only when a schema upgrade rebuilt the index.
- `meta` present on list-shaped responses:
  `{count, total, limit?, offset?, filters, time_range?}` where `count` =
  records returned, `total` = all matches in the index, `filters` = the
  applied filters as a snake_case object.
- `notices` omitted when empty. Notable codes: `sync_deferred` (lock held,
  results possibly ~2s stale), `index_migrated`, `sampled` (schema profile
  or patterns templates computed from a capped sample), `gitignore_updated`.

### `data` per command

| command | data |
|---|---|
| `search` | `{results: [entry…]}` — with `--context N`: `results: [{target, before, after}]` |
| `get` | `{results: [entry…]}` |
| `stats` | `{total_logs, by_level?, by_route?, by_user?, by_format?}` |
| `errors` | `{total_errors, total_groups, groups: [{fingerprint, error_name, error_message, template, count, affected_users, first_seen, last_seen, sample_log_ids}]}` |
| `patterns` | `{total_logs, scanned, total_templates, templates: [{template_id, template, count, share_pct, first_seen, last_seen, sample_message, sample_log_ids, levels}]}` — `template_id` (`"v1:" + sha256 prefix`) is stable for a given template string, but Drain clustering is corpus-dependent: a different window can mine different templates for the same message. For cross-run identity use the `errors` fingerprint. A `sampled` notice appears when only the newest 50k matching entries were scanned. |
| `health` | `{status: "healthy"\|"degraded"\|"unhealthy", checks: [{name, status, details, data?}]}` |
| `schema` | `{index_path, system_info, schema: {version, fields: [{name, type, count, cardinality, samples}], index_stats, supported_time_formats}}` |
| `index` | `{action: "rebuild"\|"status"\|"clean", …}` |
| `ingest` | `{source, file, entries_indexed, entries_deduplicated, entries_failed}` |
| `tap` (stderr) | `{source, journal, lines, entries_indexed, entries_failed, indexed_inline}` |
| `learn` | `{suggestions: […]}` |
| `claude` | `{installed_path, scope, updated}` |

### Log entry shape

All 16 keys always present in `json` format (null when absent); `--compact`
drops nulls; `--fields a,b` selects keys.

```
log_id, timestamp, level, source, message, request_id, user_id, route,
method, status_code, duration_ms, error{name,message,code}, source_file,
line_number, attributes{…}, parse_format
```

`attributes` holds every input field that didn't map to the core schema —
nothing is silently dropped. Scalar attribute values are full-text
searchable via `--query`.

`parse_format` (added in schema v5, always non-null) names the parser that
understood the line at ingest: `json`, `logfmt`, `apache-combined`,
`python-logging`, `go-logrus`, `log4j`, `syslog` — or `raw` when no
structured parser claimed it (see next section). Filter with
`search --parse-format <fmt>`; aggregate with `stats --by-format`.
Entries indexed before v5 report `raw` (unknown origin never counts as
structured). **Trust calibration:** a high `raw` share means field filters
(`--level`, `--request-id`, `--status`, …) see only the structured slice of
the data — prefer full-text `--query` there.

### Unstructured lines (raw capture)

Ingest is lossless. Text that matches no structured format (plain
dev-server / build / test output) is still indexed as a **raw entry**:
`message` = the text with ANSI escape sequences stripped, `level` sniffed
from it (`error`/`warn`/`debug` word or exception-class tokens like
`TypeError`/`panicked`, else `info`), `timestamp` = ingest time,
`parse_format` = `"raw"`, and the remaining fields null. Only content-free
text (blank, ANSI-only, box-drawing decoration) is skipped. Consequently
`sync.failed` / `entries_failed` count **genuine index-write errors only** —
never "unrecognized format", which no longer drops data.

**Multiline coalescing (schema v6):** physical lines are folded into
*events* before parsing. A line whose first visible character is whitespace
(stack frames, cargo code frames, `= note:` context) — or a known
continuation marker (`Caused by`) — glues to the preceding line, so one
stack trace / compiler diagnostic is ONE searchable entry, not N fragments.
Blank lines are event boundaries; blocks are capped (64 lines / 8 KB).
Structured lines are never glued: they are not indented, so they always
start their own block and parse exactly as before.

**Identity (schema v6):** entries carrying their own timestamp are keyed by
content (identical structured lines always dedup). Entries without one are
keyed by **ingest position** (`file:byte-offset`) + content: repeated
identical lines keep honest counts (50 × "retrying…" = 50 entries), while
re-reads after cursor loss and journal replays dedup exactly.

## Error envelope

```json
{
  "ok": false,
  "command": "search",
  "generated_at": "…",
  "error": {
    "code": "index_locked",
    "message": "Another process holds the index lock",
    "hint": "Stop the running watch, or pass --no-sync for a stale read.",
    "exit_code": 4
  }
}
```

`error.hint` is the agent-recovery field: when present it names the exact
next command or flag. `code` values:
`usage`, `config_invalid`, `no_sources`, `index_locked`, `index_corrupt`,
`permission_denied`, `io_error`, `parse_error`, `internal`.

## Exit codes

| code | meaning |
|---|---|
| 0 | success, results present |
| 2 | success, **zero results** (search/get/stats/errors/patterns/learn) — not a failure |
| 3 | usage or configuration error (`usage`, `config_invalid`, `no_sources`) |
| 4 | index error (`index_locked`, `index_corrupt`) |
| 5 | permission / disk / IO (`permission_denied`, `io_error`) |
| 6 | parse error (`parse_error`) |
| 1 | internal/unexpected (`internal`) |

## `--format jsonl`

Strips the envelope: one data record per line (entries for search/get,
groups for errors, the single data object for stats/health/etc.). Status
travels via exit code; the error envelope is still a single JSON line.

## Behavioral guarantees

- **Reads never block and never prompt.** If a writer holds the lock, reads
  serve the current index and attach `sync_deferred`.
- **Destructive operations require confirmation**: `index clean` prompts on
  a TTY and refuses (exit 3, hint mentions `--force`) without one.
- **Idempotence**: re-running `ingest`, re-reading rotated files, corrupt or
  deleted sync state — none of these can produce duplicate or missing
  entries (dedup by content hash `log_id`).
- **`--last <dur>` has no upper time bound**: entries with slightly-future
  timestamps (clock skew) still appear in "recent" queries.
- **Ordering**: newest-first by timestamp, always — unless
  `--sort relevance` is passed together with `--query`.
