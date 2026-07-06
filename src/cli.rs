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
        /// Pull another machine's snapshot instead of this machine's own
        /// (see `ccsync machines`).
        #[arg(long, value_name = "MACHINE")]
        from: Option<String>,
        /// Pull the snapshot as of a past commit (see `ccsync history`).
        #[arg(long, value_name = "COMMIT")]
        at: Option<String>,
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
        /// Accept incoming settings.json hook commands without confirmation.
        #[arg(long)]
        yes: bool,
        /// Restore only these top-level components (comma-separated), e.g.
        /// `--only skills,commands` or `--only settings.json`. `copilot`
        /// selects the bundled Copilot CLI tree (`~/.copilot`).
        #[arg(long, value_delimiter = ',', value_name = "COMPONENTS")]
        only: Vec<String>,
    },

    /// Show how local ~/.claude differs from the staged snapshot (or, with
    /// --remote, from the latest snapshot on the git remote).
    Diff {
        /// Compare against the remote's manifest instead of local staging
        /// (only the manifest is fetched, no snapshot data).
        #[arg(long)]
        remote: bool,
        /// With --remote: compare against another machine's snapshot.
        #[arg(long, value_name = "MACHINE", requires = "remote")]
        from: Option<String>,
    },

    /// List snapshot history on the git remote (newest first).
    History {
        /// Maximum number of commits to show.
        #[arg(long, default_value_t = 20)]
        limit: usize,
        /// Git remote URL (overrides config).
        #[arg(long)]
        remote: Option<String>,
    },

    /// List every machine with a snapshot on the git remote.
    Machines {
        /// Git remote URL (overrides config).
        #[arg(long)]
        remote: Option<String>,
    },

    /// Restore ~/.claude from a past snapshot commit (pull --at + restore).
    Rollback {
        /// Commit hash from `ccsync history`.
        commit: String,
        /// Git remote URL (overrides config).
        #[arg(long)]
        remote: Option<String>,
        /// Roll back to another machine's snapshot at that commit.
        #[arg(long, value_name = "MACHINE")]
        from: Option<String>,
        /// Restore only these top-level components (comma-separated).
        #[arg(long, value_delimiter = ',', value_name = "COMPONENTS")]
        only: Vec<String>,
        /// Accept incoming settings.json hook commands without confirmation.
        #[arg(long)]
        yes: bool,
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

    /// Manage named profiles: overlays of settings, memory, agents, skills,
    /// commands, and user-scope MCP servers over the shared base state.
    Profile {
        #[command(subcommand)]
        action: ProfileAction,
    },

    /// Manage read-only shared layers ([[layers]] in the config), e.g. a
    /// team's skills/commands repo.
    Layer {
        #[command(subcommand)]
        action: LayerAction,
    },
}

#[derive(Subcommand)]
pub enum LayerAction {
    /// List configured layers and their checkout state.
    List,
    /// Clone or update the checkout of one layer (or all configured layers).
    Pull { name: Option<String> },
    /// Copy a layer's declared components into ~/.claude (scanned and
    /// hook-gated like a restore).
    Apply {
        name: String,
        /// Accept the layer's settings.json hook commands without confirmation.
        #[arg(long)]
        yes: bool,
        /// Apply even if layer files look like they contain secrets.
        #[arg(long)]
        allow_secrets: bool,
    },
}

#[derive(Subcommand)]
pub enum ProfileAction {
    /// List profiles (the active one is marked).
    List,
    /// Create a new profile.
    Create {
        name: String,
        /// Capture the current ~/.claude components into the new profile.
        #[arg(long)]
        from_current: bool,
        /// Free-form description shown by `profile list`/`show`.
        #[arg(long)]
        description: Option<String>,
    },
    /// Capture the active profile's live edits, then swap in this profile's
    /// components (previous state is backed up; use rollback to revert).
    Switch {
        name: String,
        /// Accept the profile's settings.json hook commands without confirmation.
        #[arg(long)]
        yes: bool,
    },
    /// Show a profile's description, components, and stored MCP servers.
    Show { name: String },
    /// Show how live ~/.claude components differ from a profile's store.
    Diff { name: String },
    /// Delete a profile's store (refused while it is active).
    Delete { name: String },
    /// Revert the last switch.
    Rollback,
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
