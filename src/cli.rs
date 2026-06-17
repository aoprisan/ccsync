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
        /// List every file that would be backed up and the push target, then
        /// stop without snapshotting or pushing anything.
        #[arg(long)]
        dry_run: bool,
    },

    /// Copy this binary into a `bin` directory on your PATH and exit.
    ///
    /// Picks the first of `~/.local/bin`, `~/bin`, or `~/.cargo/bin` already on
    /// your PATH (falling back to `~/.local/bin`) and copies — rather than
    /// symlinks — so the install survives a `cargo clean` or moving the source.
    Install,

    /// Launch the interactive terminal UI.
    Tui,

    /// Run the background backup loop in the foreground.
    ///
    /// Snapshots ~/.claude on the configured interval and publishes each one to
    /// the `[service]` destination. Normally started by the installed OS
    /// service rather than invoked by hand.
    Daemon,

    /// Manage the OS background service (systemd user unit / launchd agent).
    Service {
        #[command(subcommand)]
        action: ServiceAction,
    },
}

#[derive(Subcommand)]
pub enum ServiceAction {
    /// Generate the service unit, register it, and start it.
    Install,
    /// Stop the service and remove its unit.
    Uninstall,
    /// Start the daemon detached in the background (nohup-style; no service
    /// manager required). Survives logout; logs to ~/.config/ccsync/daemon.log.
    Start,
    /// Stop a detached daemon started with `service start`.
    Stop,
    /// Report whether the service is installed and/or running.
    Status,
}
