//! ccsync — sync and back up Claude Code and Copilot CLI settings, sessions,
//! and memory.
//!
//! See `README.md` for the full workflow. In short: `snapshot` captures a
//! sanitized copy of each enabled tool directory (`~/.claude`, `~/.copilot`)
//! into a staging area, `push`/`export` transport it (git remote or encrypted
//! archive), and on another machine `pull`/`import` followed by `restore`
//! applies it with absolute-path remapping.

mod archive;
mod backups;
mod cli;
mod config;
mod error;
mod git;
mod install;
mod manifest;
mod mcp;
mod paths;
mod redact;
mod remap;
mod restore;
mod service;
mod snapshot;
mod theme;
mod tools;
mod tui;

use std::io::IsTerminal;

use anyhow::Result;
use clap::Parser;
use indicatif::{ProgressBar, ProgressStyle};

use backups::human_size;
use cli::{Cli, Command};
use config::Config;
use restore::{MergeMode, RestoreOptions};
use snapshot::{ProgressSink, SnapshotOptions};

/// Drives an [`indicatif`] progress bar from snapshot capture callbacks.
struct BarSink {
    bar: ProgressBar,
}

impl ProgressSink for BarSink {
    fn start(&self, total_files: u64, total_bytes: u64) {
        self.bar.set_length(total_bytes);
        self.bar.set_message(format!("{total_files} files"));
    }
    fn advance(&self, file_bytes: u64) {
        self.bar.inc(file_bytes);
    }
    fn finish(&self) {
        self.bar.finish_and_clear();
    }
}

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
        Command::Snapshot {
            dry_run,
            allow_secrets,
            tool,
        } => cmd_snapshot(&config, dry_run, allow_secrets, &tool),
        Command::Status { tool } => cmd_snapshot(&config, true, true, &tool),
        Command::Push { archive, remote } => cmd_push(&config, archive, remote),
        Command::Pull { archive, remote } => cmd_pull(&config, archive, remote),
        Command::Restore {
            dry_run,
            no_remap,
            overwrite,
            tool,
        } => cmd_restore(&config, dry_run, no_remap, overwrite, tool),
        Command::Export {
            file,
            allow_secrets,
        } => cmd_export(&config, &file, allow_secrets),
        Command::Import { file } => cmd_import(&file),
        Command::Backup {
            archive,
            remote,
            allow_secrets,
            dry_run,
            tool,
        } => {
            cmd_snapshot(&config, dry_run, allow_secrets, &tool)?;
            if dry_run {
                // Report the transport target without touching git or writing an
                // archive — the snapshot above already listed the files.
                if let Some(out) = archive {
                    println!("would write encrypted archive to {}", out.display());
                } else {
                    match git::resolve_remote(remote.as_deref(), config.remote.as_deref()) {
                        Ok(remote) => println!("would push snapshot to {remote}"),
                        // A missing remote is a real blocker, but in a preview we
                        // report it rather than aborting — the file list above is
                        // still useful.
                        Err(e) => println!("would push to a git remote, but {e}"),
                    }
                }
                Ok(())
            } else {
                cmd_push(&config, archive, remote)
            }
        }
        Command::Install => install::install(),
        Command::Tui => tui::run(&config),
        Command::Daemon => service::run_daemon(&config),
        Command::Service { action } => match action {
            cli::ServiceAction::Install => service::install(&config),
            cli::ServiceAction::Uninstall => service::uninstall(),
            cli::ServiceAction::Start => service::start(&config),
            cli::ServiceAction::Stop => service::stop(),
            cli::ServiceAction::Status => service::status(),
        },
    }
}

fn cmd_init(
    config_path: &std::path::Path,
    mut config: Config,
    remote: Option<String>,
) -> Result<()> {
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

fn cmd_snapshot(
    config: &Config,
    dry_run: bool,
    allow_secrets: bool,
    tool_filter: &[tools::ToolId],
) -> Result<()> {
    let staging = paths::staging_dir()?;
    let opts = SnapshotOptions::new(dry_run, allow_secrets, config);
    let plans: Vec<tools::ToolPlan> = tools::plans(config)?
        .into_iter()
        .filter(|p| tool_filter.is_empty() || tool_filter.contains(&p.id))
        .collect();

    // Show a live progress bar for real captures on an interactive terminal;
    // dry-runs and piped output fall back to the plain summary below.
    let m = if !dry_run && std::io::stderr().is_terminal() {
        let bar = ProgressBar::new(0);
        bar.set_style(
            ProgressStyle::with_template(
                "  capturing {msg} [{bar:30.cyan/blue}] {bytes}/{total_bytes}",
            )
            .expect("valid progress template")
            .progress_chars("=>-"),
        );
        let sink = BarSink { bar };
        snapshot::build_with_progress(&plans, &staging, config, &opts, &sink)?
    } else {
        snapshot::build(&plans, &staging, config, &opts)?
    };

    // Per-tool summary lines.
    for entry in &m.tools {
        let (count, total) = m
            .files
            .iter()
            .filter(|f| f.tool == entry.tool)
            .fold((0u64, 0u64), |(c, s), f| (c + 1, s + f.size));
        println!(
            "{} {count} files ({}) from {}",
            if dry_run { "would capture" } else { "captured" },
            human_size(total),
            entry.source_root
        );
    }
    if m.tools.is_empty() {
        println!(
            "{}: no tool data directories found",
            if dry_run {
                "would capture nothing"
            } else {
                "captured nothing"
            }
        );
    }
    // In dry-run, enumerate each file and the copy it implies so you can see
    // exactly what the backup will carry before anything leaves the machine.
    if dry_run {
        for f in &m.files {
            println!(
                "  copy {}/{} -> data/{}/{} ({})",
                f.tool.as_str(),
                f.rel_path,
                f.tool.as_str(),
                f.rel_path,
                human_size(f.size)
            );
        }
    }
    if !m.project_roots.is_empty() {
        println!(
            "  {} session project root(s) recorded for remapping",
            m.project_roots.len()
        );
    }
    if let Some(claude_json) = &opts.claude_json {
        if let Some(doc) = mcp::extract(claude_json)? {
            println!(
                "  {} local MCP server(s) bundled from {}",
                mcp::server_count(&doc),
                claude_json.display()
            );
        }
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
    println!(
        "  staged at {} — run `ccsync restore` to apply",
        staging.display()
    );
    Ok(())
}

fn cmd_restore(
    config: &Config,
    dry_run: bool,
    no_remap: bool,
    overwrite: bool,
    tool_filter: Vec<tools::ToolId>,
) -> Result<()> {
    let staging = paths::staging_dir()?;
    let opts = RestoreOptions {
        dry_run,
        remap: !no_remap,
        merge: if overwrite {
            MergeMode::Overwrite
        } else {
            MergeMode::Merge
        },
        claude_json: if config.include_mcp_servers {
            paths::claude_json_file().ok()
        } else {
            None
        },
        tools: tool_filter,
    };
    let report = restore::run(&staging, config, &opts)?;

    if !report.mappings.is_empty() {
        println!("path remapping:");
        for m in &report.mappings {
            println!("  {} -> {}", m.from, m.to);
        }
    }
    for (tool, backup) in &report.backups {
        println!(
            "backed up existing {} to {}",
            paths::tool_dir(*tool)?.display(),
            backup.display()
        );
    }
    if let Some(backup) = &report.claude_json_backup {
        println!("backed up existing ~/.claude.json to {}", backup.display());
    }
    // Per-tool restored counts.
    for &tool in tools::ToolId::all() {
        let prefix = format!("{}/", tool.as_str());
        let count = report
            .files_written
            .iter()
            .filter(|f| f.starts_with(&prefix))
            .count();
        if count > 0 {
            println!(
                "{} {count} files to {}",
                if dry_run { "would restore" } else { "restored" },
                paths::tool_dir(tool)?.display()
            );
        }
    }
    if report.files_written.is_empty() {
        println!("nothing to restore");
    }
    if report.mcp_servers_restored > 0 {
        println!(
            "{} {} local MCP server(s) into ~/.claude.json",
            if dry_run { "would merge" } else { "merged" },
            report.mcp_servers_restored
        );
    }
    Ok(())
}

fn cmd_export(config: &Config, file: &std::path::Path, allow_secrets: bool) -> Result<()> {
    let pass = archive::passphrase_from_env()?;
    cmd_snapshot(config, false, allow_secrets, &[])?;
    let staging = paths::staging_dir()?;
    archive::create(&staging, file, &pass)?;
    println!("wrote encrypted archive to {}", file.display());
    Ok(())
}

fn cmd_import(file: &std::path::Path) -> Result<()> {
    let pass = archive::passphrase_from_env()?;
    let staging = paths::staging_dir()?;
    archive::extract(file, &staging, &pass)?;
    println!(
        "imported snapshot to {} — run `ccsync restore` to apply",
        staging.display()
    );
    Ok(())
}
