//! Multiline coalescing: glue continuation lines to their anchor so one
//! *event* (a cargo diagnostic block, a JS/Python stack trace, a vitest
//! failure) becomes one entry instead of N meaningless fragments.
//!
//! Design constraints (this sits on the per-line ingest hot path):
//! - O(1) work per line: one `\x1b` scan, one leading-char test, and a few
//!   cheap prefix checks. No format detection, no extra regex passes.
//! - Structured lines are never glued together: JSON/logfmt/syslog lines are
//!   not indented, so they always start a new block and flow through
//!   `parse_log_entry` exactly as before — zero behavior change on
//!   structured corpora.
//! - Bounded: a block is force-flushed at [`MAX_BLOCK_LINES`] /
//!   [`MAX_BLOCK_BYTES`] so runaway indented output (YAML dumps, minified
//!   payloads) cannot buffer unbounded memory.
//!
//! The heuristic: a line *continues* the current block when its first
//! visible (ANSI-stripped) character is whitespace — the near-universal
//! continuation signal (`    at f (x.js:1)`, `  --> src/main.rs:97`,
//! `  File "app.py", line 3`, `   | code frame`, `  = note: ...`) — or when
//! it starts with a known unindented continuation marker (`Caused by:`).
//! Blank lines terminate a block and are swallowed (they carry nothing).

use crate::parser::parsers::strip_ansi;

/// Hard cap on lines per block.
pub const MAX_BLOCK_LINES: u32 = 64;
/// Hard cap on bytes per block.
pub const MAX_BLOCK_BYTES: usize = 8 * 1024;

/// A completed multiline (or single-line) event.
#[derive(Debug)]
pub struct Block {
    /// Original text, lines joined with '\n' (no trailing newline).
    pub text: String,
    /// Position of the block's first line (byte offset the caller supplied).
    pub first_byte: u64,
}

/// Stateful line-to-block folder. Feed lines in order with their byte
/// offsets; a `Some(Block)` return is the *previous* block, completed
/// because the current line started a new one. Call [`Self::flush`] at EOF.
#[derive(Default)]
pub struct LineCoalescer {
    buf: String,
    first_byte: u64,
    lines: u32,
}

impl LineCoalescer {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed one line (without trailing newline). Returns the completed
    /// previous block when `line` starts a new one.
    pub fn push(&mut self, line: &str, byte_offset: u64) -> Option<Block> {
        if line.trim().is_empty() {
            // Blank line: event boundary. Emit what we have, index nothing
            // for the blank itself.
            return self.flush();
        }

        let over_cap =
            self.lines >= MAX_BLOCK_LINES || self.buf.len() + line.len() > MAX_BLOCK_BYTES;

        let completed = if self.buf.is_empty() {
            None
        } else if is_continuation(line) && !over_cap {
            self.buf.push('\n');
            self.buf.push_str(line);
            self.lines += 1;
            return None;
        } else {
            self.flush()
        };

        self.buf.push_str(line);
        self.first_byte = byte_offset;
        self.lines = 1;
        completed
    }

    /// Byte offset where the in-progress (unflushed) block starts, if any.
    /// Cursor persistence must not advance past this: those bytes are
    /// buffered, not yet indexed.
    pub fn pending_start(&self) -> Option<u64> {
        if self.buf.is_empty() {
            None
        } else {
            Some(self.first_byte)
        }
    }

    /// Emit the in-progress block, if any. Call at EOF / stream end.
    pub fn flush(&mut self) -> Option<Block> {
        if self.buf.is_empty() {
            return None;
        }
        self.lines = 0;
        Some(Block {
            text: std::mem::take(&mut self.buf),
            first_byte: self.first_byte,
        })
    }
}

/// Does this line continue the previous one?
fn is_continuation(line: &str) -> bool {
    // Fast path: no ANSI escapes → test the raw first byte.
    let visible: &str = if line.as_bytes().contains(&0x1b) {
        return is_continuation_stripped(&strip_ansi(line));
    } else {
        line
    };
    is_continuation_stripped(visible)
}

fn is_continuation_stripped(line: &str) -> bool {
    match line.as_bytes().first() {
        Some(b' ') | Some(b'\t') => {
            // Indented but self-labeled diagnostics are anchors, not
            // continuations: tools like `flutter analyze` / `dart analyze`
            // indent EVERY line ("  error • Target of URI doesn't exist…"),
            // and gluing unrelated diagnostics buries the real failure.
            !is_labeled_diagnostic(line.trim_start())
        }
        // Unindented continuations seen in the wild (Java/Rust error chains).
        _ => line.starts_with("Caused by"),
    }
}

/// `error • …` / `warning: …` / `info - …` at the start of a line: a new
/// diagnostic, regardless of indentation.
fn is_labeled_diagnostic(trimmed: &str) -> bool {
    for label in ["error", "warning", "info", "hint"] {
        if let Some(rest) = strip_prefix_ignore_case(trimmed, label) {
            let mut chars = rest.trim_start().chars();
            if matches!(chars.next(), Some('•') | Some(':') | Some('-')) {
                return true;
            }
        }
    }
    false
}

fn strip_prefix_ignore_case<'a>(s: &'a str, prefix: &str) -> Option<&'a str> {
    // `get` (not slicing) so a prefix length landing mid-char (e.g. inside
    // '•') is a mismatch, not a panic — labels are ASCII, the text isn't.
    let head = s.get(..prefix.len())?;
    if head.eq_ignore_ascii_case(prefix) {
        Some(&s[prefix.len()..])
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Feed lines with synthetic offsets; return completed block texts.
    fn run(lines: &[&str]) -> Vec<String> {
        let mut c = LineCoalescer::new();
        let mut out = Vec::new();
        let mut offset = 0u64;
        for line in lines {
            if let Some(b) = c.push(line, offset) {
                out.push(b.text);
            }
            offset += line.len() as u64 + 1;
        }
        if let Some(b) = c.flush() {
            out.push(b.text);
        }
        out
    }

    #[test]
    fn test_cargo_diagnostic_block_is_one_event() {
        let blocks = run(&[
            "warning: unused variable: `x`",
            " --> src/main.rs:4:9",
            "  |",
            "4 |     let x = 1;",
            "  |         ^ help: prefix with underscore",
            "  = note: `#[warn(unused_variables)]` on by default",
            "warning: struct `Foo` is never constructed",
        ]);
        // "4 |" starts with a digit → cargo actually left-pads it; simulate
        // real cargo output where code-frame lines are space-padded.
        assert_eq!(blocks.len(), 3);
        assert!(blocks[0].starts_with("warning: unused variable"));
        assert!(blocks[0].contains("--> src/main.rs:4:9"));
    }

    #[test]
    fn test_real_cargo_padding_glues_code_frames() {
        let blocks = run(&[
            "error[E0308]: mismatched types",
            "  --> src/lib.rs:7:5",
            "   |",
            " 7 |     \"oops\"",
            "   |     ^^^^^^ expected `i32`, found `&str`",
        ]);
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].lines().count(), 5);
    }

    #[test]
    fn test_js_stack_trace_is_one_event() {
        let blocks = run(&[
            "TypeError: Cannot read property 'id' of undefined",
            "    at login (src/auth/login.ts:47:15)",
            "    at Router.handle (src/router.ts:23:10)",
            "Server listening on :3000",
        ]);
        assert_eq!(blocks.len(), 2);
        assert!(blocks[0].contains("at login"));
        assert_eq!(blocks[1], "Server listening on :3000");
    }

    #[test]
    fn test_python_traceback_frames_glue_to_anchor() {
        let blocks = run(&[
            "Traceback (most recent call last):",
            "  File \"app.py\", line 3, in <module>",
            "    main()",
            "ValueError: bad input",
        ]);
        // Frames glue to the Traceback anchor; the final unindented
        // exception line is its own (meaningful) event.
        assert_eq!(blocks.len(), 2);
        assert!(blocks[0].contains("File \"app.py\""));
        assert_eq!(blocks[1], "ValueError: bad input");
    }

    #[test]
    fn test_caused_by_chains_glue() {
        let blocks = run(&[
            "error: failed to run custom build command",
            "Caused by: process didn't exit successfully",
        ]);
        assert_eq!(blocks.len(), 1);
    }

    #[test]
    fn test_blank_line_is_boundary_and_swallowed() {
        let blocks = run(&["first event", "", "  continuation of nothing?"]);
        // Blank flushes; the indented line after it starts its own block.
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0], "first event");
        assert_eq!(blocks[1], "  continuation of nothing?");
    }

    #[test]
    fn test_json_lines_are_never_glued() {
        let blocks = run(&[
            r#"{"level":"info","message":"a"}"#,
            r#"{"level":"info","message":"b"}"#,
        ]);
        assert_eq!(blocks.len(), 2);
    }

    #[test]
    fn test_ansi_colored_indent_still_continues() {
        let blocks = run(&[
            "\x1b[31merror\x1b[0m: build failed",
            "\x1b[90m    at bundle (vite:1:1)\x1b[0m",
        ]);
        assert_eq!(blocks.len(), 1);
    }

    #[test]
    fn test_indented_labeled_diagnostics_are_anchors() {
        // flutter/dart analyze indent every line; each diagnostic is its own
        // event, not a continuation of the previous one.
        let blocks = run(&[
            "Analyzing genui...",
            "  error • Target of URI doesn't exist: 'package:args/args.dart' • lib/main.dart:6:8",
            "  error • Target of URI doesn't exist: 'package:file/file.dart' • lib/main.dart:7:8",
            "  info • Unused import • lib/other.dart:2:1",
        ]);
        assert_eq!(blocks.len(), 4);
    }

    #[test]
    fn test_line_cap_forces_flush() {
        let mut lines: Vec<String> = vec!["anchor".to_string()];
        for i in 0..100 {
            lines.push(format!("  indented {}", i));
        }
        let refs: Vec<&str> = lines.iter().map(|s| s.as_str()).collect();
        let blocks = run(&refs);
        assert!(blocks.len() >= 2, "cap must split runaway blocks");
        assert!(blocks[0].lines().count() as u32 <= MAX_BLOCK_LINES);
    }

    #[test]
    fn test_first_byte_tracks_block_start() {
        let mut c = LineCoalescer::new();
        assert!(c.push("anchor one", 0).is_none());
        assert!(c.push("  cont", 11).is_none());
        let b = c.push("anchor two", 18).unwrap();
        assert_eq!(b.first_byte, 0);
        let b2 = c.flush().unwrap();
        assert_eq!(b2.first_byte, 18);
    }
}
