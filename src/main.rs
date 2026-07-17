//! somethingsoff CLI - local-first log search for developers and AI agents
//!
//! Indexes structured logs with Tantivy and keeps the index fresh
//! transparently: read commands auto-ingest new log data before answering.

#![warn(clippy::unwrap_used, clippy::expect_used)]
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

use anyhow::Result;
use clap::{Parser, Subcommand};

// The ingest path allocates heavily (per-line String parsing/redaction/doc
// building); mimalloc cuts allocator overhead versus the system allocator.
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

use somethingsoff::cmd::{
    ClaudeCommand, ErrorsCommand, GetCommand, HealthCommand, IndexCommand, IngestCommand,
    LearnCommand, SchemaCommand, SearchCommand, StatsCommand, TapCommand, WatchCommand,
};
use somethingsoff::output::OutputFormat;

#[derive(Parser)]
#[command(name = "somethingsoff")]
#[command(author)]
#[command(version)]
#[command(
    about = "Local-first log search and analysis — zero setup, agent-friendly",
    long_about = "
somethingsoff — local-first log search for developers and AI agents

Zero setup: put log files in ./logs/ (or `somethingsoff ingest` any file) and
query. Read commands transparently ingest new log data before answering, so
results are always fresh — no daemon, no rebuild step, nothing to configure.
Project state lives in ./.somethingsoff/ (index, sync cursors, config).

QUICK START:
  somethingsoff search --level error --last 24h
  somethingsoff errors --last 1h            # grouped error analysis
  somethingsoff get --request-id req-123    # trace one request
  somethingsoff stats --by-level --by-route
  somethingsoff search -q \"connection refused\" --context 5

OUTPUT CONTRACT (stable v1):
  Every command prints one JSON envelope on stdout:
    {\"ok\", \"command\", \"version\", \"generated_at\", \"elapsed_ms\",
     \"sync\", \"data\", \"meta\", \"notices\"}
  Failures print {\"ok\":false, \"error\":{\"code\",\"message\",\"hint\",\"exit_code\"}}.
  --format jsonl strips the envelope: one data record per line.
  Diagnostics go to stderr only (silence with --quiet).

EXIT CODES:
  0 ok · 2 ok but zero results · 3 usage/config · 4 index locked/corrupt ·
  5 permission/IO · 6 parse error · 1 internal

MORE:
  app 2>&1 | somethingsoff tap       # capture any process's output (no SDK)
  somethingsoff watch                # optional: continuous low-latency ingest
  somethingsoff schema               # discover fields, sources, index stats
  somethingsoff learn --sample '..'  # suggest a regex for an unknown format
  somethingsoff claude install       # teach Claude Code to use this tool
  PII (emails, tokens, cards, secrets) is redacted at ingest by default;
  disable with --no-redact."
)]
struct Cli {
    /// Suppress stderr output
    #[arg(long, global = true)]
    pub quiet: bool,

    /// Disable PII redaction (passwords, emails, API keys will be stored as-is)
    #[arg(long, global = true)]
    pub no_redact: bool,

    /// Skip the automatic ingest of new log data before read commands
    #[arg(long, global = true)]
    pub no_sync: bool,

    /// Output format: json (envelope) or jsonl (one record per line)
    #[arg(long, global = true, value_enum, default_value = "json")]
    pub format: OutputFormat,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
// One instance parsed per process; the size skew between variants is
// irrelevant here and boxing would only add noise.
#[allow(clippy::large_enum_variant)]
enum Commands {
    /// Search logs with filters
    Search(SearchCommand),
    /// Get logs by specific ID (log_id hash, request, user)
    Get(GetCommand),
    /// Show aggregated statistics
    Stats(StatsCommand),
    /// Aggregate and analyze error logs
    Errors(ErrorsCommand),
    /// Check system and index health
    Health(HealthCommand),
    /// Manage the search index
    Index(IndexCommand),
    /// Ingest a log file into the index
    Ingest(IngestCommand),
    /// Pipe a process through: echo + capture (`app 2>&1 | somethingsoff tap`)
    Tap(TapCommand),
    /// Continuously ingest new log entries (optional; reads auto-sync anyway)
    #[command(alias = "serve")]
    Watch(WatchCommand),
    /// Get schema and index discovery information
    Schema(SchemaCommand),
    /// Suggest regex patterns from a sample log line
    Learn(LearnCommand),
    /// Set up Claude Code integration (installs the Agent Skill)
    Claude(ClaudeCommand),
}

impl Commands {
    fn name(&self) -> &'static str {
        match self {
            Commands::Search(_) => "search",
            Commands::Get(_) => "get",
            Commands::Stats(_) => "stats",
            Commands::Errors(_) => "errors",
            Commands::Health(_) => "health",
            Commands::Index(_) => "index",
            Commands::Ingest(_) => "ingest",
            Commands::Tap(_) => "tap",
            Commands::Watch(_) => "watch",
            Commands::Schema(_) => "schema",
            Commands::Learn(_) => "learn",
            Commands::Claude(_) => "claude",
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    // Contract: argument errors are usage errors — exit 3 with a JSON error
    // envelope. Clap's default (human text, exit 2) would collide with
    // "ok but zero results" and give agents a false success signal.
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(e) => {
            use clap::error::ErrorKind;
            if matches!(e.kind(), ErrorKind::DisplayHelp | ErrorKind::DisplayVersion) {
                e.exit(); // help/version are not errors
            }
            let message = e
                .to_string()
                .lines()
                .next()
                .unwrap_or("invalid arguments")
                .trim_start_matches("error: ")
                .to_string();
            let cli_error = somethingsoff::output::CliError::new(
                somethingsoff::output::ErrorCode::Usage,
                message,
            )
            .with_hint(
                "Run `somethingsoff --help` for usage. Pass values starting with '-' \
                         as --flag=value.",
            );
            println!("{}", somethingsoff::output::render_error("cli", &cli_error));
            std::process::exit(cli_error.code.exit_code());
        }
    };
    somethingsoff::set_quiet(cli.quiet);
    somethingsoff::set_no_redact(cli.no_redact);
    somethingsoff::set_no_sync(cli.no_sync);
    somethingsoff::set_output_format(cli.format);

    let command_name = cli.command.name();

    let result = match cli.command {
        Commands::Search(cmd) => cmd.execute().await,
        Commands::Get(cmd) => cmd.execute().await,
        Commands::Stats(cmd) => cmd.execute().await,
        Commands::Errors(cmd) => cmd.execute().await,
        Commands::Health(cmd) => cmd.execute(),
        Commands::Index(cmd) => cmd.execute().await,
        Commands::Ingest(cmd) => cmd.execute().await,
        Commands::Tap(cmd) => cmd.execute().await,
        Commands::Watch(cmd) => {
            cmd.execute().await?;
            Ok(0)
        }
        Commands::Schema(cmd) => cmd.execute().await,
        Commands::Learn(cmd) => cmd.execute().await,
        Commands::Claude(cmd) => cmd.execute().await,
    };

    match result {
        Ok(code) => std::process::exit(code as i32),
        Err(e) => {
            let cli_error = somethingsoff::output::classify_error(&e);
            println!(
                "{}",
                somethingsoff::output::render_error(command_name, &cli_error)
            );
            std::process::exit(cli_error.code.exit_code());
        }
    }
}
