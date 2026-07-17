pub mod cmd;
pub mod config;
pub mod error;
pub mod index;
pub mod output;
pub mod parser;
pub mod pii;
pub mod schema;
pub mod sync;

use std::sync::atomic::{AtomicBool, Ordering};

static QUIET: AtomicBool = AtomicBool::new(false);
static NO_REDACT: AtomicBool = AtomicBool::new(false);
static NO_SYNC: AtomicBool = AtomicBool::new(false);
static JSONL: AtomicBool = AtomicBool::new(false);

/// Set the global quiet flag
pub fn set_quiet(quiet: bool) {
    QUIET.store(quiet, Ordering::SeqCst);
}

/// Check if the global quiet flag is set
pub fn is_quiet() -> bool {
    QUIET.load(Ordering::SeqCst)
}

/// Set the global no-redact flag
pub fn set_no_redact(no_redact: bool) {
    NO_REDACT.store(no_redact, Ordering::SeqCst);
}

/// Check if PII redaction is disabled
pub fn is_no_redact() -> bool {
    NO_REDACT.load(Ordering::SeqCst)
}

/// Set the global no-sync flag (skip auto-ingest before reads)
pub fn set_no_sync(no_sync: bool) {
    NO_SYNC.store(no_sync, Ordering::SeqCst);
}

/// Check if auto-ingest-before-read is disabled
pub fn is_no_sync() -> bool {
    NO_SYNC.load(Ordering::SeqCst)
}

/// Set the global output format (from the `--format` flag)
pub fn set_output_format(format: output::OutputFormat) {
    JSONL.store(format == output::OutputFormat::Jsonl, Ordering::SeqCst);
}

/// True when `--format jsonl` was requested (envelope-stripped records)
pub fn is_jsonl() -> bool {
    JSONL.load(Ordering::SeqCst)
}

#[macro_export]
macro_rules! log_info {
    ($($arg:tt)*) => {
        if !$crate::is_quiet() {
            eprintln!("[INFO] {}", format!($($arg)*));
        }
    };
}

#[macro_export]
macro_rules! log_warn {
    ($($arg:tt)*) => {
        if !$crate::is_quiet() {
            eprintln!("[WARN] {}", format!($($arg)*));
        }
    };
}

#[macro_export]
macro_rules! log_error {
    ($($arg:tt)*) => {
        if !$crate::is_quiet() {
            eprintln!("[ERROR] {}", format!($($arg)*));
        }
    };
}
