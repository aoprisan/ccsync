//! ccsync — sync and back up Claude Code settings, sessions, and memory.
//!
//! See `README.md` for the full workflow. In short: `snapshot` captures a
//! sanitized copy of `~/.claude` into a staging area, `push`/`export` transport
//! it (git remote or encrypted archive), and on another machine `pull`/`import`
//! followed by `restore` applies it with absolute-path remapping.

mod archive;
mod backups;
mod cli;
mod config;
mod error;
mod git;
mod manifest;
mod paths;
mod redact;
mod remap;
mod restore;
mod service;
mod snapshot;
mod theme;
mod tui;

use anyhow::Result;
use clap::Parser;

use cli::{Cli, Command};
use config::Config;
use restore::{MergeMode, RestoreOptions};
use snapshot::SnapshotOptions;

fn main() {
    if let Err(e) = run() {
        eprintln!("error: {e:#}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let cli = Cli::parse();
    let config_path = paths::config_file()?;
    let config = Config::load(&config_path)?;

    match cli.command {
        Command::Init { remote } => cmd_init(&config_path, config, remote),
        Command::Snapshot { dry_run, allow_secrets } => {
            cmd_snapshot(&config, dry_run, allow_secrets)
        }
        Command::Status => cmd_snapshot(&config, true, true),
        Command::Push { archive, remote } => cmd_push(&config, archive, remote),
        Command::Pull { archive, remote } => cmd_pull(&config, archive, remote),
        Command::Restore { dry_run, no_remap, overwrite } => {
            cmd_restore(&config, dry_run, no_remap, overwrite)
        }
        Command::Export { file, allow_secrets } => cmd_export(&config, &file, allow_secrets),
        Command::Import { file } => cmd_import(&file),
        Command::Backup { archive, remote, allow_secrets } => {
            cmd_snapshot(&config, false, allow_secrets)?;
            cmd_push(&config, archive, remote)
        }
        Command::Tui => tui::run(&config),
        Command::Daemon => service::run_daemon(&config),
        Command::Service { action } => match action {
            cli::ServiceAction::Install => service::install(&config),
            cli::ServiceAction::Uninstall => service::uninstall(),
            cli::ServiceAction::Status => service::status(),
        },
    }
}

fn cmd_init(config_path: &std::path::Path, mut config: Config, remote: Option<String>) -> Result<()> {
    if remote.is_some() {
        config.remote = remote;
    }
    config.save(config_path)?;
    println!("wrote config to {}", config_path.display());
    if config.remote.is_none() {
        println!("tip: set a remote with `ccsync init --remote <git-url>` to enable git sync");
    }
    Ok(())
}

fn cmd_snapshot(config: &Config, dry_run: bool, allow_secrets: bool) -> Result<()> {
    let claude = paths::claude_dir()?;
    let staging = paths::staging_dir()?;
    let opts = SnapshotOptions { dry_run, allow_secrets };
    let m = snapshot::build(&claude, &staging, config, &opts)?;

    let total: u64 = m.files.iter().map(|f| f.size).sum();
    println!(
        "{} {} files ({:.1} KiB) from {}",
        if dry_run { "would capture" } else { "captured" },
        m.files.len(),
        total as f64 / 1024.0,
        claude.display()
    );
    if !m.project_roots.is_empty() {
        println!("  {} session project root(s) recorded for remapping", m.project_roots.len());
    }
    if !dry_run {
        println!("  staged at {}", staging.display());
    }
    Ok(())
}

fn cmd_push(
    config: &Config,
    archive_path: Option<std::path::PathBuf>,
    remote: Option<String>,
) -> Result<()> {
    let staging = paths::staging_dir()?;
    snapshot::require_staged(&staging)?;

    if let Some(out) = archive_path {
        let pass = archive::passphrase_from_env()?;
        archive::create(&staging, &out, &pass)?;
        println!("wrote encrypted archive to {}", out.display());
    } else {
        let remote = git::resolve_remote(remote.as_deref(), config.remote.as_deref())?;
        git::push(&remote, &staging)?;
        println!("pushed snapshot to {remote}");
    }
    Ok(())
}

fn cmd_pull(
    config: &Config,
    archive_path: Option<std::path::PathBuf>,
    remote: Option<String>,
) -> Result<()> {
    let staging = paths::staging_dir()?;

    if let Some(input) = archive_path {
        let pass = archive::passphrase_from_env()?;
        archive::extract(&input, &staging, &pass)?;
        println!("imported snapshot from {}", input.display());
    } else {
        let remote = git::resolve_remote(remote.as_deref(), config.remote.as_deref())?;
        git::pull(&remote, &staging)?;
        println!("pulled snapshot from {remote}");
    }
    println!("  staged at {} — run `ccsync restore` to apply", staging.display());
    Ok(())
}

fn cmd_restore(config: &Config, dry_run: bool, no_remap: bool, overwrite: bool) -> Result<()> {
    let claude = paths::claude_dir()?;
    let staging = paths::staging_dir()?;
    let opts = RestoreOptions {
        dry_run,
        remap: !no_remap,
        merge: if overwrite { MergeMode::Overwrite } else { MergeMode::Merge },
    };
    let report = restore::run(&claude, &staging, config, &opts)?;

    if !report.mappings.is_empty() {
        println!("path remapping:");
        for m in &report.mappings {
            println!("  {} -> {}", m.from, m.to);
        }
    }
    if let Some(backup) = &report.backup_dir {
        println!("backed up existing {} to {}", claude.display(), backup.display());
    }
    println!(
        "{} {} files to {}",
        if dry_run { "would restore" } else { "restored" },
        report.files_written.len(),
        claude.display()
    );
    Ok(())
}

fn cmd_export(config: &Config, file: &std::path::Path, allow_secrets: bool) -> Result<()> {
    let pass = archive::passphrase_from_env()?;
    cmd_snapshot(config, false, allow_secrets)?;
    let staging = paths::staging_dir()?;
    archive::create(&staging, file, &pass)?;
    println!("wrote encrypted archive to {}", file.display());
    Ok(())
}

fn cmd_import(file: &std::path::Path) -> Result<()> {
    let pass = archive::passphrase_from_env()?;
    let staging = paths::staging_dir()?;
    archive::extract(file, &staging, &pass)?;
    println!("imported snapshot to {} — run `ccsync restore` to apply", staging.display());
    Ok(())
}
