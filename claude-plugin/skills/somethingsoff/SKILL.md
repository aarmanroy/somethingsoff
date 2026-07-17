---
name: somethingsoff
description: Search and analyze the project's application logs with the somethingsoff CLI. Use when debugging errors, investigating why a request failed, tracing a request ID, checking what an app logged, watching for new errors, or asking "what happened" in a dev server, backend, or test run. Also use for vague runtime-misbehavior reports — "something's broken", "it's not working", "the app is acting weird", "it crashed", "this endpoint is slow", flaky or intermittent tests, or any bug report where the cause isn't known yet. Scope, reads runtime logs only — not useful for compile errors or purely static code questions. Works with zero setup on any log format.
metadata:
  version: "1.2"
---

# Searching application logs with somethingsoff

`somethingsoff` is a local-first log index. **Zero setup**: read commands
auto-discover log files in `./logs/`, create the index, and ingest new data
before answering — never run an ingest or rebuild step first. Results are
always fresh. Nothing leaves the machine.

Run `somethingsoff --help` for the full command reference before asking the
user how something works.

## When to reach for this

Treat this as the **cheap first diagnostic step** for any runtime
misbehavior — even a vague report like "something's off with the app". Run

```bash
somethingsoff --quiet errors --last 15m
```

*before* reading source code. It costs one command: exit `2` means the logs
hold nothing recent (you lost five seconds — move on); exit `0` hands you
grouped failures with timestamps and request IDs to pull on. Evidence first,
code second. Skip this only when the problem is clearly compile-time or
purely static.

## Recipes (copy-paste)

```bash
# What's broken right now? (grouped errors, newest window first)
somethingsoff --quiet errors --last 1h

# Trace one request across all sources
somethingsoff --quiet get --request-id req-123

# Full-text search, bounded output
somethingsoff --quiet search -q "connection refused" --last 24h -n 20 --compact

# What happened around each failure? (5 log lines before/after)
somethingsoff --quiet search --level error --last 1h --context 5

# Slow requests / HTTP failures
somethingsoff --quiet search --slow-above 1000 --last 1h
somethingsoff --quiet search --status 500-599 --last 1h

# Overview: volumes by level/route, what fields exist to filter on
somethingsoff --quiet stats --by-level --by-route
somethingsoff --quiet schema

# Capture a dev server the user is about to start (any language, no SDK)
npm run dev 2>&1 | somethingsoff tap --source web
```

## Output contract

Every command prints one JSON envelope on stdout:
`{ok, command, version, generated_at, elapsed_ms, sync, data, meta, notices}`.
Results live in `data`; pagination and applied filters in `meta`
(`meta.total` = all matches, `meta.count` = returned). Failures print
`{ok:false, error:{code, message, hint, exit_code}}` — **read `error.hint`;
it names the exact next command to run.**

Exit codes: `0` ok · `2` ok but **zero results** (not a failure — widen the
time window or drop filters) · `3` usage/config · `4` index locked/corrupt ·
`5` permission/IO · `6` parse error · `1` internal.

## Token economy

- Always pass `--quiet` (clean stderr) and bound output: `-n 20`, `--last 1h`.
- `--fields timestamp,level,message --compact` trims entries to essentials.
- `--format jsonl` emits bare records, one per line (no envelope) — best for
  piping into further filtering.
- `errors` groups near-duplicate failures by masked `template`
  (`Connection timeout to db-<num> after <num>ms`) — prefer it over raw
  search when triaging; one group line replaces hundreds of entries.
- **Trust calibration:** `stats --by-format` shows how much of the index was
  structurally parsed vs captured as `raw` text. A high `raw` share means
  field filters (`--level`, `--request-id`, `--status`) only see part of the
  data — fall back to full-text `-q` there. Inspect the unparsed lines with
  `search --parse-format raw`.

## Cautions

- PII (emails, tokens, cards, secrets) is redacted at ingest by default.
  Only use `--no-redact` if the user explicitly asks for raw values.
- `index clean` deletes data; it requires `--force` in non-interactive use.
  Prefer `--dry-run` first.
- A `notices[].code == "sync_deferred"` means another writer holds the lock
  (a `watch` or `tap` is running); results may lag by ~2s. That is normal.
