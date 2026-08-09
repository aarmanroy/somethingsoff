# Changelog

All notable changes to this project are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/); versions follow
[Semantic Versioning](https://semver.org/).

## [0.4.0] - 2026-08-09

Three of these were confident wrong answers rather than visible failures,
which for a tool whose index an LLM reads is the worst failure mode there
is. All were found by auditing against real logs, and each now has a
regression test that the previous suite was structurally unable to write.

### Fixed

- **`search` returned the wrong documents on any real index.** Ordering
  used `order_by_u64_field` against the `timestamp` *string* field, which
  sorts by segment-local term ordinal. On a single-segment index — which
  is what every test built — that looks correct. On a 3-segment index, a
  newest-first query returned the *oldest* batch. Ordering now uses the
  `timestamp_ms` u64 fast field that already existed in the schema.

- **Every Java/log4j line was invisible to every time filter.** The parser
  appended `.000` to a time that already carried millis, producing
  `10:00:01.500.000Z`; normalization rejected it, so the entry got no
  `timestamp_ms`. `search --level error` found 2 entries and
  `search --level error --last 24h` found 0. The parser and detector also
  required exactly three fractional digits, so `%d{yyyy-MM-dd HH:mm:ss}`
  and micro/nanosecond precision fell through to the raw fallback and lost
  their timestamps entirely.

- **An unnormalizable timestamp was stored verbatim**, leaving the entry
  with no `timestamp_ms` — present in totals, unreachable by every time
  query. A time-only `01:57:10.385` (Next.js writes these) was indexed,
  reported as valid JSON by `stats --by-format`, and could not be found by
  `--last 100000h`. Such entries now fall back to ingest time with the
  original preserved in `attributes.original_timestamp`.

- **PII redaction failed open on essentially every credential minted after
  ~2015.** Every pattern was keyword-anchored, firing only next to
  `token=` or `password:`, so bare credentials passed through verbatim:
  `ghp_`, `github_pat_`, `glpat-`, `sk-ant-`, `sk-proj-`,
  `sk_live_`/`rk_test_`, `xoxb-`, `AIza`, `npm_`, `hf_`, raw JWTs and PEM
  private key blocks. These are now matched on shape alone. URL
  credentials are redacted while keeping the scheme and host legible.

- **`tap` wrote the unredacted line to disk.** The journal at
  `.somethingsoff/streams/*.jsonl` was written before anything parsed it,
  making it the one place a secret outlived redaction. It is now redacted
  and created `0600`. Stdout passthrough stays verbatim.

- **Redaction destroyed real content.** A missing word boundary meant
  `pass` matched inside ordinary words, indexing `compass=north` as
  `compass [REDACTED]`.

### Added

- **Discovery finds the logs projects actually write.** The scan was
  exactly `./logs/*.{log,json,jsonl}`, one level deep — three of eleven
  real-world path shapes. Now also: `log/` (Rails, Django, Java), nested
  files recursively, `.ndjson`/`.txt`/`.out`/`.err`, rotated files
  (`app.log.1`, sharing the live file's source name), and root-level
  `*.log`. Compressed rotations are skipped; recursion is bounded (depth
  4, 512 files) so a symlink cycle stops rather than spins. The root scan
  takes `.log` only, so `package.json` is never mistaken for a log.

- **A Rails parser.** A 285k-line `log/development.log` indexed as 100%
  `raw`, so `--request-id`, `--status` and `--slow-above` returned
  nothing and the request UUID polluted every mined error template.
  Extracts the tagged request id, method and route, status and duration.

- **`SOMETHINGSOFF_HOOK_DEBUG=1`** traces each hook decision to stderr,
  which Claude Code surfaces under `claude --debug`. The hook exits 0 in
  silence on every path with nothing to say, which made "no errors in the
  window", "rate-limited" and "broken" indistinguishable.

- **`.somethingsoff/` now creates a `.gitignore` excluding itself**, so a
  directory of indexed log content cannot be committed by accident.

### Changed

- `SCHEMA_VERSION` 6 → 7. Existing indexes rebuild transparently on the
  next read; sources are on disk, so this costs one slower query.

- The hook payload is parsed field by field rather than through a derived
  `Deserialize`. One field arriving with an unexpected type used to
  discard the whole event and disable the hook permanently and silently.
  The failure text is accepted under `error`, `error_message` or
  `failure_reason`.

## [0.3.0] - 2026-07-30

### Added

- **`PreToolUse` hook — the indexed answer instead of the raw log.** When
  Claude reaches for a log this project indexes (`cat logs/app.log`,
  `tail -f`, `grep`, or the `Read` tool), the hook has *already run the
  query* and returns the result as the denial reason: error count for the
  last hour, the top group's masked template, its count and last-seen time,
  and the exact follow-up commands. On a 43 KB dev-server log that is
  ~10,600 tokens of raw text replaced by ~140 — with the actual failing
  function named. Measured across Python, Node, Go, Rust, nginx, Docker and
  Java logs: 95–98% of the context saved.

  It is deliberately hard to trigger, because a hook that interrupts
  wrongly is worse than no hook:

  - **Only real sources.** Paths are resolved through the same discovery
    the sync engine uses, so `/var/log/system.log`, `npm-debug.log` and
    anything else this project doesn't index runs untouched.
  - **Scoped to the file asked for.** Reading `logs/auth.json` reports
    `auth`'s errors, never a louder neighbour's, and the suggested
    follow-up carries the matching `--source`.
  - **Small files are just read.** Below 4 KiB the raw file costs less
    context than a summary of it and says more, so it passes through. A
    `tail -f` is intercepted at any size — it never returns on its own.
  - **Search patterns aren't paths.** `grep -rn 'logs/app.log' src/`
    searches source code and is left alone.
  - **Never a trap.** Repeating a denied read always runs it; the last 16
    denials are tracked, so alternating phrasings can't corner the agent.
  - **Never noise.** Malformed payloads, a missing index, a held lock, a
    query over budget — every one exits 0 in silence and the command runs.
    No first-ever ingest is ever triggered from inside a hook.

  Overhead on a Bash call that reads no log: ~5 ms.

### Changed

- The plugin's `PreToolUse` matcher is now `Bash|Read`; `PostToolUseFailure`
  remains Bash-only.

## [0.2.0] - 2026-07-20

### Added

- **`patterns`**: mine recurring message templates across all log levels
  using a native [Drain](https://github.com/logpai/Drain3) implementation —
  the "what is the app logging?" triage counterpart to `errors`. Collapses
  noisy windows into a ranked list of templates (`<*>` marks generalized
  tokens) with per-level counts, share percentages, first/last seen, and
  sample log IDs. Templates are recomputed per query; a `sampled` notice
  appears when only the newest 50k matching entries were scanned.
- **`ingest` positional file argument**: `somethingsoff ingest <FILE>` now
  works without `--file`, and `--source` defaults to the lowercased file
  stem.

### Changed

- Clap usage errors now surface the failing subcommand's usage line in the
  error hint, so agents can self-correct without a `--help` round-trip.

## [0.1.0] - 2026-07-17

First public release.

### Added

- **Zero-setup log search**: read commands (`search`, `errors`, `get`,
  `stats`, `schema`) auto-discover `./logs/`, index only new bytes into a
  local [Tantivy](https://github.com/quickwit-oss/tantivy) index, and answer
  in milliseconds. No daemon, no config, nothing leaves your machine.
- **`tap`**: pipe any process through (`npm run dev 2>&1 | somethingsoff tap`)
  — passthrough + journal + index, zero instrumentation, any language.
- **Lossless ingest**: 8 structured formats auto-detected per line (JSON,
  logfmt, syslog RFC3164/5424, Apache/Nginx, Python logging, Go logrus,
  Log4j) and *everything else* captured as raw entries (ANSI stripped, level
  sniffed) — dev-server, build, and test output included.
- **Multiline coalescing**: stack traces and compiler diagnostics index as
  one searchable event, not N fragments.
- **Error intelligence**: `errors` groups near-duplicate failures by masked
  template with deterministic ordering.
- **Stable v1 output contract** (`docs/CONTRACT.md`): one JSON envelope on
  stdout, machine-readable error codes + recovery hints, documented exit
  codes — built to be driven by AI coding agents.
- **Claude Code integration, first-class**: this repo is a plugin
  marketplace. The plugin ships the `/somethingsoff` Agent Skill plus a
  `PostToolUseFailure` hook that injects the top indexed error group into
  Claude's context the moment a command fails. `somethingsoff claude install`
  remains the skill-only, offline fallback.
- **PII redaction at ingest** (emails, tokens, cards, secrets) by default.
