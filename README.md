# somethingsoff

**When something's off, ask your logs.** Local-first log search for developers and AI coding agents — zero setup, no daemon, nothing leaves your machine.

[![Crates.io](https://img.shields.io/crates/v/somethingsoff.svg)](https://crates.io/crates/somethingsoff)
[![CI](https://github.com/aarmanroy/somethingsoff/actions/workflows/rust-ci.yml/badge.svg)](https://github.com/aarmanroy/somethingsoff/actions/workflows/rust-ci.yml)
[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](LICENSE)

Your app writes logs; `somethingsoff` makes them instantly searchable. Read commands transparently discover your log files, index only the new bytes, and answer in milliseconds.

```bash
cd your-project        # has a ./logs/ dir? that's the whole setup
somethingsoff errors --last 1h
```

Built for the debugging loop — especially the one your AI coding agent runs: ask many small questions ("is this error new?", "did my fix work?", "show me everything for request X") and get fresh, structured, token-efficient answers every time.

## Install

```bash
# Prebuilt binary (macOS arm64/x64, Linux x64/arm64, Windows x64)
curl --proto '=https' --tlsv1.2 -LsSf https://github.com/aarmanroy/somethingsoff/releases/latest/download/somethingsoff-installer.sh | sh

# Or via cargo
cargo binstall somethingsoff   # prebuilt, seconds
cargo install somethingsoff    # from source
```

## Claude Code

Make Claude reach for your logs the moment a command fails. This repo is a plugin marketplace — two commands inside Claude Code:

```
/plugin marketplace add aarmanroy/somethingsoff
/plugin install somethingsoff@somethingsoff
```

The plugin ships two things:

- **The `/somethingsoff` skill** — teaches Claude the CLI, the output contract, and when to check logs first ("something's off with the app" is literally in its trigger vocabulary). Type `/somethingsoff` to invoke it explicitly, or let Claude reach for it on its own.
- **A failure hook** — when a Bash command fails in a session, the hook queries your index in-process and injects the *actual* top error group into Claude's context: `"Payment gateway gw-<num> timeout after <num>ms" (6×, last seen 09:35)`. Evidence at the exact moment debugging starts. Silent when there's nothing useful to say, rate-limited, and it never runs a first-time ingest on its own.

No plugin? `somethingsoff claude install` writes the same skill to `.claude/skills/` (or `--global`) — offline, no marketplace. Pick one path, not both.

## Quickstart

**Option 1 — log files.** Drop (or point your app's file logger at) `./logs/*.log|json|jsonl`, then just query:

```bash
somethingsoff search --level error --last 24h
somethingsoff errors --last 1h                  # grouped error analysis
somethingsoff get --request-id req-123          # trace one request
somethingsoff stats --by-level --by-route
```

**Option 2 — pipe anything.** Works with any language, zero instrumentation. Your terminal still shows the output:

```bash
npm run dev 2>&1 | somethingsoff tap --source web
cargo run    2>&1 | somethingsoff tap --source backend
python app.py 2>&1 | somethingsoff tap
```

## How it works

- **Auto-ingest-on-query**: every read command checks your sources first (a stat per file — ~10ms when nothing changed), tails only new bytes into a [Tantivy](https://github.com/quickwit-oss/tantivy) full-text index, then answers. No `ingest`/`rebuild` step, no watcher required.
- **Formats auto-detected per line**: JSON, logfmt, Python logging, syslog (RFC3164/5424), Apache/Nginx, Go logrus, Log4j. Mixed formats in one file are fine.
- **Lossless — nothing is dropped**: unknown JSON fields (and camelCase aliases like `requestId`, `msg`, `durationMs`) map into the schema or land in a searchable `attributes` object. Any line that matches *no* structured format — plain dev-server, build, or test output — is still captured as a raw text entry (ANSI stripped, level sniffed), and multiline stack traces / compiler diagnostics coalesce into single events.
- **Error intelligence**: `errors` groups near-duplicate failures by masked template — `Connection timeout to db-<num> after <num>ms: 438×` instead of 438 lines.
- **State is disposable**: entries are deduplicated by content hash, so a lost cursor file or a re-ingested file can never produce duplicates or wrong results.
- **Private by default**: emails, tokens, cards, and secrets are redacted at ingest (disable with `--no-redact`). Everything lives in `./.somethingsoff/` (index, cursors, tap journals, optional config).

## Commands

| Command | What it does |
|---|---|
| `search` | Filter + full-text query (`-q`, `--level`, `--last`, `--status 500-599`, `--slow-above 1000`, `--context 5`, ...) |
| `errors` | Error groups by masked template: count, affected users, first/last seen |
| `get` | Point lookup by `LOG_ID`, `--request-id`, or `--user-id` |
| `stats` | Volumes `--by-level` / `--by-route` / `--by-user` / `--by-format` |
| `schema` | Profile of what's actually in your logs: fields, types, cardinality, samples |
| `tap` | Pipe a process through: echo + capture + index |
| `watch` | Optional continuous ingest (lower latency than on-demand sync) |
| `ingest` | One-shot ingest of a file outside `./logs/` (stays auto-synced after) |
| `health` | Index, sources, disk, and lock checks |
| `index` | `rebuild` / `status` / `clean` (retention) |
| `learn` | Suggest a regex for an unrecognized log format |
| `claude` | `install` the Agent Skill · `hook` (plugin plumbing) |

Global flags: `--quiet` (silence stderr diagnostics) · `--format json|jsonl` · `--no-sync` (skip auto-ingest) · `--no-redact`.

## Output contract (stable v1)

Every command prints exactly one JSON envelope on stdout; diagnostics go to stderr. Failures carry a machine-readable `code` and a `hint` naming the next command to run. Exit codes: `0` ok · `2` ok but zero results · `3` usage/config · `4` index locked/corrupt · `5` permission/IO · `6` parse · `1` internal.

The full specification lives in [docs/CONTRACT.md](docs/CONTRACT.md) — it's the file agents and tests both rely on.

## For AI agents

This CLI is designed to be driven by coding agents:

- One envelope shape everywhere; branch on exit codes; recover via `error.hint`.
- Output is bounded by default (`-n`, `--last`); trim further with `--fields timestamp,level,message --compact`, or stream bare records with `--format jsonl`.
- `schema` is self-describing discovery: run it first to learn what fields exist.
- `stats --by-format` is trust calibration: it shows how much of the index was structurally parsed vs captured raw, so an agent knows when field filters see everything and when to fall back to full-text.
- Non-interactive by design: no prompts without a TTY (destructive ops require `--force`), reads never block on locks (stale read + `sync_deferred` notice instead).

## Configuration (optional)

Zero-config covers most projects. For anything else, `./.somethingsoff/config.toml`:

```toml
[general]
retention_days = 30            # `index clean` deletes older entries

[log_sources]                  # explicit sources beyond ./logs/
backend = "/var/log/myapp/backend.log"

[sync]
auto = true                    # auto-ingest before reads
poll_interval_secs = 2         # `watch` polling interval
```

Set `SOMETHINGSOFF_BASE_DIR` to relocate the state directory (defaults to `./.somethingsoff`).

## Development

```bash
cargo test                          # unit + integration suites
cargo clippy --all-targets -- -D warnings
cargo bench                         # criterion benchmarks
```

## License

MIT
