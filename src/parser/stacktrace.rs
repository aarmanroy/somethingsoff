//! Stack trace parsing helpers.
//!
//! Extracts structured frames from common JavaScript/TypeScript, Python, and
//! Rust-style stack traces so ingestion can backfill `source_file` and
//! `line_number` even when logs only contain a raw stack string.

use once_cell::sync::Lazy;
use regex::Regex;
use serde::Serialize;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct StackFrame {
    pub function_name: Option<String>,
    pub file_path: String,
    pub line_number: usize,
    pub column_number: Option<usize>,
}

static JS_FRAME_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(
        r#"^\s*at\s+(?:(?P<function>.+?)\s+\()?(?P<file>[^():\s][^():]*?):(?P<line>\d+):(?P<column>\d+)\)?$"#,
    )
    .unwrap()
});

static PYTHON_FRAME_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(
        r#"^\s*File\s+"(?P<file>[^"]+)",\s+line\s+(?P<line>\d+)(?:,\s+in\s+(?P<function>.+))?$"#,
    )
    .unwrap()
});

static RUST_FRAME_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r#"^\s*at\s+(?P<file>[^:\s][^:]*?):(?P<line>\d+):(?P<column>\d+)$"#).unwrap()
});

/// Parse a raw stack trace into structured frames.
pub fn parse_stack_trace(stack: &str) -> Vec<StackFrame> {
    stack.lines().filter_map(parse_frame_line).collect()
}

fn parse_frame_line(line: &str) -> Option<StackFrame> {
    if let Some(caps) = JS_FRAME_RE.captures(line) {
        return Some(StackFrame {
            function_name: caps.name("function").map(|m| m.as_str().trim().to_string()),
            file_path: caps.name("file")?.as_str().to_string(),
            line_number: caps.name("line")?.as_str().parse().ok()?,
            column_number: caps.name("column")?.as_str().parse().ok(),
        });
    }

    if let Some(caps) = PYTHON_FRAME_RE.captures(line) {
        return Some(StackFrame {
            function_name: caps.name("function").map(|m| m.as_str().trim().to_string()),
            file_path: caps.name("file")?.as_str().to_string(),
            line_number: caps.name("line")?.as_str().parse().ok()?,
            column_number: None,
        });
    }

    if let Some(caps) = RUST_FRAME_RE.captures(line) {
        return Some(StackFrame {
            function_name: None,
            file_path: caps.name("file")?.as_str().to_string(),
            line_number: caps.name("line")?.as_str().parse().ok()?,
            column_number: caps.name("column")?.as_str().parse().ok(),
        });
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_javascript_stack_trace() {
        let stack = "TypeError: Boom\n    at login (src/auth/login.ts:47:15)\n    at router (src/router.ts:23:10)";

        let frames = parse_stack_trace(stack);

        assert_eq!(frames.len(), 2);
        assert_eq!(frames[0].function_name.as_deref(), Some("login"));
        assert_eq!(frames[0].file_path, "src/auth/login.ts");
        assert_eq!(frames[0].line_number, 47);
        assert_eq!(frames[0].column_number, Some(15));
    }

    #[test]
    fn test_parse_python_stack_trace() {
        let stack = "Traceback (most recent call last):\n  File \"/app/main.py\", line 12, in handler\n  File \"/app/db.py\", line 34, in connect";

        let frames = parse_stack_trace(stack);

        assert_eq!(frames.len(), 2);
        assert_eq!(frames[0].function_name.as_deref(), Some("handler"));
        assert_eq!(frames[0].file_path, "/app/main.py");
        assert_eq!(frames[0].line_number, 12);
        assert_eq!(frames[0].column_number, None);
    }

    #[test]
    fn test_parse_rust_style_frame() {
        let stack = "panic occurred\n             at src/main.rs:42:5";

        let frames = parse_stack_trace(stack);

        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].file_path, "src/main.rs");
        assert_eq!(frames[0].line_number, 42);
        assert_eq!(frames[0].column_number, Some(5));
    }

    #[test]
    fn test_parse_unknown_stack_trace_returns_empty() {
        let frames = parse_stack_trace("completely opaque text");
        assert!(frames.is_empty());
    }
}
