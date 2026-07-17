# Changelog

All notable changes to this project are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/); versions follow
[Semantic Versioning](https://semver.org/).

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
