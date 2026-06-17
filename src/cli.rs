//! Command-line surface, defined with clap's derive API.

use std::path::PathBuf;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "ccsync",
    version,
    about = "Sync and back up Claude Code settings, sessions, and memory across machines"
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand)]
pub enum Command {
    /// Write a default config file to ~/.config/ccsync/config.toml.
    Init {
        /// Set the git remote URL for sync/backup.
        #[arg(long)]
        remote: Option<String>,
    },

    /// Build a sanitized snapshot of ~/.claude into the staging directory.
    Snapshot {
        /// Report what would be captured without writing anything.
        #[arg(long)]
        dry_run: bool,
        /// Include config files even if they look like they contain secrets.
        #[arg(long)]
        allow_secrets: bool,
    },

    /// Show what a snapshot would capture (alias for `snapshot --dry-run`).
    Status,

    /// Publish the staged snapshot to a git remote (default) or an archive.
    Push {
        /// Write an encrypted archive to this path instead of pushing to git.
        #[arg(long, value_name = "FILE")]
        archive: Option<PathBuf>,
        /// Git remote URL (overrides config).
        #[arg(long)]
        remote: Option<String>,
    },

    /// Fetch a snapshot from a git remote (default) or an archive into staging.
    Pull {
        /// Read an encrypted archive from this path instead of pulling from git.
        #[arg(long, value_name = "FILE")]
        archive: Option<PathBuf>,
        /// Git remote URL (overrides config).
        #[arg(long)]
        remote: Option<String>,
    },

    /// Apply the staged snapshot to the local ~/.claude (backs up first).
    Restore {
        /// Show what would change without writing anything.
        #[arg(long)]
        dry_run: bool,
        /// Restore transcripts verbatim without remapping absolute paths.
        #[arg(long)]
        no_remap: bool,
        /// Replace config files wholesale instead of deep-merging JSON.
        #[arg(long)]
        overwrite: bool,
    },

    /// One-shot: snapshot ~/.claude and write an encrypted archive.
    Export {
        /// Output archive path (e.g. claude-backup.tar.gz.age).
        file: PathBuf,
        #[arg(long)]
        allow_secrets: bool,
    },

    /// Read an encrypted archive into the staging directory.
    Import {
        /// Input archive path.
        file: PathBuf,
    },

    /// Convenience: snapshot then push (to git unless --archive is given).
    Backup {
        #[arg(long, value_name = "FILE")]
        archive: Option<PathBuf>,
        #[arg(long)]
        remote: Option<String>,
        #[arg(long)]
        allow_secrets: bool,
    },

    /// Launch the interactive terminal UI.
    Tui,
}
