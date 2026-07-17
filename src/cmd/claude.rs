//! Claude Code integration: `somethingsoff claude install` writes the
//! bundled Agent Skill so Claude discovers and drives this CLI on its own.
//!
//! The skill is embedded in the binary (`include_str!`), so installation
//! works offline and the skill version always matches the binary.

use anyhow::{Context, Result};
use clap::{Args, Subcommand};
use std::path::PathBuf;

use crate::output::{CliError, Envelope, ErrorCode};

/// The bundled skill — the same file the Claude Code plugin ships
/// (`claude-plugin/`), so the plugin and `claude install` can never drift.
const SKILL_MD: &str = include_str!("../../claude-plugin/skills/somethingsoff/SKILL.md");

/// Set up Claude Code integration
#[derive(Args)]
pub struct ClaudeCommand {
    #[command(subcommand)]
    pub command: ClaudeSubcommand,
}

#[derive(Subcommand)]
pub enum ClaudeSubcommand {
    /// Install the somethingsoff Agent Skill for Claude Code
    Install(InstallOptions),
    /// Claude Code hook entrypoint (plumbing: hook event JSON on stdin)
    Hook,
}

#[derive(Args)]
pub struct InstallOptions {
    /// Install for all your projects (~/.claude/skills/) instead of this
    /// project's .claude/skills/
    #[arg(long)]
    pub global: bool,
}

impl ClaudeCommand {
    pub async fn execute(&self) -> Result<u8> {
        match &self.command {
            ClaudeSubcommand::Install(options) => install(options),
            // Infallible: hook stdout belongs to Claude Code's hook
            // protocol, so it must never fall through to the error
            // envelope in main().
            ClaudeSubcommand::Hook => Ok(crate::cmd::claude_hook::run().await),
        }
    }
}

fn install(options: &InstallOptions) -> Result<u8> {
    let mut envelope = Envelope::new("claude");

    let skills_dir = if options.global {
        let home = dirs::home_dir().ok_or_else(|| {
            CliError::new(ErrorCode::IoError, "Cannot determine your home directory").with_hint(
                "Run without --global to install into this project's .claude/skills/ instead.",
            )
        })?;
        home.join(".claude").join("skills").join("somethingsoff")
    } else {
        PathBuf::from(".claude")
            .join("skills")
            .join("somethingsoff")
    };

    let skill_path = skills_dir.join("SKILL.md");
    let existing = std::fs::read_to_string(&skill_path).ok();
    let updated = match &existing {
        Some(current) => current != SKILL_MD,
        None => true,
    };

    if updated {
        std::fs::create_dir_all(&skills_dir)
            .with_context(|| format!("Failed to create skills directory: {:?}", skills_dir))?;
        std::fs::write(&skill_path, SKILL_MD)
            .with_context(|| format!("Failed to write skill: {:?}", skill_path))?;
    }

    // Project installs: make sure the index/state dir never gets committed.
    if !options.global {
        match ensure_gitignored() {
            Ok(true) => envelope.notice(
                "gitignore_updated",
                "Added .somethingsoff/ to .gitignore",
                None,
            ),
            Ok(false) => {}
            Err(e) => envelope.notice(
                "gitignore_skipped",
                &format!("Could not update .gitignore: {}", e),
                Some("Add `.somethingsoff/` to your .gitignore manually."),
            ),
        }
    }

    crate::log_info!(
        "Skill {} at {}",
        if updated {
            "installed"
        } else {
            "already up to date"
        },
        skill_path.display()
    );
    crate::log_info!(
        "Claude Code picks it up automatically (new top-level skills may need a session restart)"
    );

    envelope.emit(
        serde_json::json!({
            "installed_path": skill_path.display().to_string(),
            "scope": if options.global { "global" } else { "project" },
            "updated": updated,
        }),
        None,
    )?;
    Ok(0)
}

/// Append `.somethingsoff/` to ./.gitignore when inside a git repo and the
/// pattern is missing. Returns Ok(true) if the file was changed.
fn ensure_gitignored() -> Result<bool> {
    if !std::path::Path::new(".git").exists() {
        return Ok(false);
    }
    let gitignore = std::path::Path::new(".gitignore");
    let current = std::fs::read_to_string(gitignore).unwrap_or_default();
    let already = current.lines().any(|l| {
        matches!(
            l.trim(),
            ".somethingsoff" | ".somethingsoff/" | "/.somethingsoff"
        )
    });
    if already {
        return Ok(false);
    }
    let mut updated = current;
    if !updated.is_empty() && !updated.ends_with('\n') {
        updated.push('\n');
    }
    updated.push_str(".somethingsoff/\n");
    std::fs::write(gitignore, updated).context("Failed to write .gitignore")?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_bundled_skill_has_required_frontmatter() {
        assert!(SKILL_MD.starts_with("---\n"));
        assert!(SKILL_MD.contains("name: somethingsoff"));
        assert!(SKILL_MD.contains("description:"));
        // The description must carry trigger phrases for auto-discovery.
        assert!(SKILL_MD.contains("debugging errors"));
        // Body teaches the contract essentials.
        assert!(SKILL_MD.contains("error.hint"));
        assert!(SKILL_MD.contains("--quiet"));
    }
}
