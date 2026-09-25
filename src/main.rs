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
mod diff;
mod error;
mod git;
mod install;
mod layer;
mod manifest;
mod mcp;
mod paths;
mod profile;
mod redact;
mod remap;
mod restore;
mod service;
mod snapshot;
mod theme;
mod tui;

use std::io::IsTerminal;

use anyhow::{Context, Result};
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
    // `[machines.<id>]` overrides are folded in here for every command; code
    // that persists config must reload from disk first (see cmd_push).
    let config = Config::load(&config_path)?.with_machine_overrides();

    match cli.command {
        Command::Init { remote } => cmd_init(&config_path, config, remote),
        Command::Snapshot {
            dry_run,
            allow_secrets,
        } => cmd_snapshot(&config, dry_run, allow_secrets),
        // A true alias for `snapshot --dry-run`: secrets are scanned so status
        // reports exactly what a real snapshot would do.
        Command::Status => cmd_snapshot(&config, true, false),
        Command::Push { archive, remote } => cmd_push(&config_path, config, archive, remote),
        Command::Pull {
            archive,
            remote,
            from,
            at,
        } => cmd_pull(&config, archive, remote, from, at),
        Command::History { limit, remote } => cmd_history(&config, limit, remote),
        Command::Machines { remote } => cmd_machines(&config, remote),
        Command::Rollback {
            commit,
            remote,
            from,
            only,
            yes,
        } => {
            cmd_pull(&config, None, remote, from, Some(commit))?;
            cmd_restore(&config, false, false, false, yes, only)
        }
        Command::Restore {
            dry_run,
            no_remap,
            overwrite,
            yes,
            only,
        } => cmd_restore(&config, dry_run, no_remap, overwrite, yes, only),
        Command::Diff { remote, from } => cmd_diff(&config, remote, from),
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
        } => {
            cmd_snapshot(&config, dry_run, allow_secrets)?;
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
                cmd_push(&config_path, config, archive, remote)
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
        Command::Profile { action } => cmd_profile(&config, action),
        Command::Layer { action } => cmd_layer(&config, action),
    }
}

fn cmd_layer(config: &Config, action: cli::LayerAction) -> Result<()> {
    use cli::LayerAction;

    let layers_root = paths::layers_dir()?;
    match action {
        LayerAction::List => {
            if config.layers.is_empty() {
                println!("no layers configured; add a [[layers]] entry to the config, e.g.");
                println!("  [[layers]]");
                println!("  name = \"team\"");
                println!("  remote = \"git@github.com:acme/claude-shared.git\"");
                println!("  components = [\"skills\", \"commands\"]");
                return Ok(());
            }
            for l in &config.layers {
                let state = if layers_root.join(&l.name).is_dir() {
                    "pulled"
                } else {
                    "not pulled"
                };
                println!(
                    "  {}  —  {} ({}), components: {}",
                    l.name,
                    l.remote,
                    state,
                    l.components.join(", ")
                );
            }
            Ok(())
        }
        LayerAction::Pull { name } => {
            let targets: Vec<_> = match &name {
                Some(n) => vec![layer::find(config, n)?],
                None => config.layers.iter().collect(),
            };
            if targets.is_empty() {
                println!("no layers configured; nothing to pull");
                return Ok(());
            }
            for l in targets {
                layer::pull(l, &layers_root)?;
                println!("pulled layer {:?} from {}", l.name, l.remote);
            }
            Ok(())
        }
        LayerAction::Apply {
            name,
            yes,
            allow_secrets,
        } => {
            let l = layer::find(config, &name)?;
            let claude = paths::claude_dir()?;
            let applied = layer::apply(
                l,
                &layers_root,
                &claude,
                config.confirm_hooks && !yes,
                allow_secrets,
            )?;
            println!(
                "applied {} file(s) from layer {name:?} into {}",
                applied.len(),
                claude.display()
            );
            Ok(())
        }
    }
}

fn cmd_profile(config: &Config, action: cli::ProfileAction) -> Result<()> {
    use cli::ProfileAction;

    let root = paths::profiles_dir()?;
    let claude = paths::claude_dir()?;
    let claude_json = if config.profiles.include_user_mcp {
        paths::claude_json_file().ok()
    } else {
        None
    };
    let live = profile::LiveState {
        claude_dir: &claude,
        claude_json: claude_json.as_deref(),
    };

    match action {
        ProfileAction::List => {
            let names = profile::list(&root)?;
            if names.is_empty() {
                println!("no profiles yet; `ccsync profile create <name> --from-current`");
                return Ok(());
            }
            let active = profile::active(&root)?.map(|a| a.name);
            for name in names {
                let marker = if active.as_deref() == Some(&name) {
                    "* "
                } else {
                    "  "
                };
                let desc = profile::read_meta(&root, &name)?.description;
                if desc.is_empty() {
                    println!("{marker}{name}");
                } else {
                    println!("{marker}{name} — {desc}");
                }
            }
            Ok(())
        }
        ProfileAction::Create {
            name,
            from_current,
            description,
        } => {
            let captured = profile::create(
                &root,
                &name,
                description,
                config,
                from_current.then_some(&live),
            )?;
            if from_current {
                println!("created profile {name:?} with {captured} file(s) from current state");
            } else {
                println!("created empty profile {name:?}");
            }
            Ok(())
        }
        ProfileAction::Switch { name, yes } => {
            let report =
                profile::switch(&root, &name, &live, config, config.confirm_hooks && !yes)?;
            if let Some(from) = &report.from {
                if from == &report.to {
                    println!(
                        "already on {:?}; captured {} live file(s) into its store",
                        report.to, report.captured_files
                    );
                    return Ok(());
                }
                println!(
                    "captured {} file(s) back into {:?}",
                    report.captured_files, from
                );
            }
            if let Some(backup) = &report.backup_dir {
                println!("backed up previous state to {}", backup.display());
            }
            println!(
                "switched to {:?}: {} file(s) applied, {} user-scope MCP server(s)",
                report.to, report.applied_files, report.mcp_servers
            );
            Ok(())
        }
        ProfileAction::Show { name } => {
            if !profile::exists(&root, &name) {
                anyhow::bail!("profile {name:?} does not exist");
            }
            let meta = profile::read_meta(&root, &name)?;
            let active = profile::active(&root)?.map(|a| a.name);
            println!(
                "{name}{}",
                if active.as_deref() == Some(&name) {
                    " (active)"
                } else {
                    ""
                }
            );
            if !meta.description.is_empty() {
                println!("  {}", meta.description);
            }
            let comps = profile::components(&root, &name, config)?;
            let data = profile::profile_dir(&root, &name).join("data");
            for comp in comps {
                let present = data.join(&comp).exists();
                println!("  {} {comp}", if present { "+" } else { "-" });
            }
            let mcp_count = profile::stored_mcp_count(&root, &name);
            if mcp_count > 0 {
                println!("  + {mcp_count} user-scope MCP server(s)");
            }
            Ok(())
        }
        ProfileAction::Diff { name } => {
            let entries = profile::diff_live(&root, &name, &live, config)?;
            if entries.is_empty() {
                println!("live state matches profile {name:?}");
                return Ok(());
            }
            for e in &entries {
                let tag = match e.state {
                    diff::DiffState::LocalOnly => "live only   ",
                    diff::DiffState::OtherOnly => "profile only",
                    diff::DiffState::Changed => "differs     ",
                };
                println!("  {tag} {}", e.rel);
            }
            println!(
                "{} difference(s); `ccsync profile switch {name}` captures live state back when {name:?} is active",
                entries.len()
            );
            Ok(())
        }
        ProfileAction::Delete { name } => {
            profile::delete(&root, &name)?;
            println!("deleted profile {name:?}");
            Ok(())
        }
        ProfileAction::Rollback { yes } => {
            let msg = profile::rollback(&root, &live, config, config.confirm_hooks && !yes)?;
            println!("{msg}");
            Ok(())
        }
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

fn cmd_snapshot(config: &Config, dry_run: bool, allow_secrets: bool) -> Result<()> {
    let claude = paths::claude_dir()?;
    let staging = paths::staging_dir()?;
    let opts = SnapshotOptions::new(dry_run, allow_secrets, config);

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
        snapshot::build_with_progress(&claude, &staging, config, &opts, &sink)?
    } else {
        snapshot::build(&claude, &staging, config, &opts)?
    };

    let total: u64 = m.files.iter().map(|f| f.size).sum();
    println!(
        "{} {} files ({}) from {}",
        if dry_run { "would capture" } else { "captured" },
        m.files.len(),
        human_size(total),
        claude.display()
    );
    // In dry-run, enumerate each file and the copy it implies so you can see
    // exactly what the backup will carry before anything leaves the machine.
    if dry_run {
        for f in &m.files {
            println!(
                "  copy {} -> data/{} ({})",
                f.rel_path,
                f.rel_path,
                human_size(f.size)
            );
        }
    }
    if m.redacted_spans > 0 {
        println!(
            "  {} secret-shaped span(s) redacted from transcripts in the staged copy",
            m.redacted_spans
        );
    }
    if !m.project_roots.is_empty() {
        println!(
            "  {} session project root(s) recorded for remapping",
            m.project_roots.len()
        );
    }
    let unclassified = snapshot::unclassified_top_level(&claude, config);
    if !unclassified.is_empty() {
        println!(
            "  warning: not classified by include/exclude (never synced): {}",
            unclassified.join(", ")
        );
        println!("    add them to `include` or `exclude` in the config to silence this");
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
    config_path: &std::path::Path,
    config: Config,
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
        // Persist the machine identity on first push so a later hostname
        // change doesn't fork this machine's history under a new subtree.
        // Saved from a freshly-loaded config: `config` has machine overrides
        // folded in and must never be written back.
        let machine_id = config.effective_machine_id();
        if config.machine_id.is_none() {
            if let Ok(mut fresh) = Config::load(config_path) {
                fresh.machine_id = Some(machine_id.clone());
                if fresh.save(config_path).is_ok() {
                    println!("recorded machine_id = {machine_id:?} in the config");
                }
            }
        }
        let remote = git::resolve_remote(remote.as_deref(), config.remote.as_deref())?;
        if let Some(from) = pulled_from(&staging) {
            anyhow::bail!(
                "staging holds a pulled snapshot ({from}), not this machine's state; \
                 pushing it would overwrite machines/{machine_id} with it. \
                 Run `ccsync snapshot` (or `ccsync backup`) first"
            );
        }
        git::push(&remote, &staging, &machine_id)?;
        println!("pushed snapshot to {remote} (machine {machine_id})");
    }
    Ok(())
}

/// Record that `staging` now holds a pulled/imported snapshot (see
/// [`paths::pulled_marker`]), naming where it came from.
fn mark_pulled(staging: &std::path::Path, source: &str) -> Result<()> {
    std::fs::write(paths::pulled_marker(staging), source)
        .with_context(|| format!("marking {} as pulled", staging.display()))
}

/// Where the staged snapshot came from, when it was pulled rather than taken
/// on this machine.
fn pulled_from(staging: &std::path::Path) -> Option<String> {
    std::fs::read_to_string(paths::pulled_marker(staging))
        .ok()
        .map(|s| s.trim().to_string())
}

fn cmd_pull(
    config: &Config,
    archive_path: Option<std::path::PathBuf>,
    remote: Option<String>,
    from: Option<String>,
    at: Option<String>,
) -> Result<()> {
    let staging = paths::staging_dir()?;

    if let Some(input) = archive_path {
        if from.is_some() || at.is_some() {
            anyhow::bail!("--from/--at select git snapshots and cannot combine with --archive");
        }
        let pass = archive::passphrase_from_env()?;
        archive::extract(&input, &staging, &pass)?;
        mark_pulled(&staging, &format!("archive {}", input.display()))?;
        println!("imported snapshot from {}", input.display());
    } else {
        let remote = git::resolve_remote(remote.as_deref(), config.remote.as_deref())?;
        let own_id = config.effective_machine_id();
        match &at {
            Some(commit) => {
                git::pull_at(&remote, commit, &staging, from.as_deref(), &own_id)?;
                mark_pulled(&staging, &format!("{remote} at {commit}"))?;
                println!("pulled snapshot at {commit} from {remote}");
            }
            None => {
                git::pull(&remote, &staging, from.as_deref(), &own_id)?;
                mark_pulled(&staging, &remote)?;
                println!("pulled snapshot from {remote}");
            }
        }
    }
    println!(
        "  staged at {} — run `ccsync restore` to apply",
        staging.display()
    );
    Ok(())
}

fn cmd_history(config: &Config, limit: usize, remote: Option<String>) -> Result<()> {
    // `git::log` reads the local cache; refresh it from the remote first so
    // history shows other machines' pushes too.
    let remote = git::resolve_remote(remote.as_deref(), config.remote.as_deref())?;
    git::refresh_cache(&remote)?;
    let commits = git::log(limit)?;
    if commits.is_empty() {
        println!("no snapshot history yet; `ccsync backup` creates the first commit");
        return Ok(());
    }
    for (hash, date, subject) in commits {
        println!("{hash}  {date}  {subject}");
    }
    println!("restore one with `ccsync rollback <commit>` (or `ccsync pull --at <commit>`)");
    Ok(())
}

fn cmd_machines(config: &Config, remote: Option<String>) -> Result<()> {
    let remote = git::resolve_remote(remote.as_deref(), config.remote.as_deref())?;
    let own_id = config.effective_machine_id();
    let machines = git::machines(&remote)?;
    if machines.is_empty() {
        println!("no machine snapshots on {remote} yet");
        return Ok(());
    }
    for (name, m) in machines {
        let marker = if name == own_id { "* " } else { "  " };
        println!(
            "{marker}{name}  —  host {}, {} file(s), {}, ccsync {}",
            m.source_host,
            m.files.len(),
            m.created_at,
            m.ccsync_version
        );
    }
    println!("pull another machine's snapshot with `ccsync pull --from <machine>`");
    Ok(())
}

fn cmd_restore(
    config: &Config,
    dry_run: bool,
    no_remap: bool,
    overwrite: bool,
    yes: bool,
    only: Vec<String>,
) -> Result<()> {
    let claude = paths::claude_dir()?;
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
        confirm_hooks: config.confirm_hooks && !yes,
        components: if only.is_empty() { None } else { Some(only) },
        profiles_root: if config.profiles.sync {
            paths::profiles_dir().ok()
        } else {
            None
        },
    };
    let report = restore::run(&claude, &staging, config, &opts)?;

    if !report.mappings.is_empty() {
        println!("path remapping:");
        for m in &report.mappings {
            println!("  {} -> {}", m.from, m.to);
        }
    }
    if let Some(backup) = &report.backup_dir {
        println!(
            "backed up existing {} to {}",
            claude.display(),
            backup.display()
        );
    }
    if let Some(backup) = &report.claude_json_backup {
        println!("backed up existing ~/.claude.json to {}", backup.display());
    }
    println!(
        "{} {} files to {}",
        if dry_run { "would restore" } else { "restored" },
        report.files_written.len(),
        claude.display()
    );
    if report.mcp_servers_restored > 0 {
        println!(
            "{} {} local MCP server(s) into ~/.claude.json",
            if dry_run { "would merge" } else { "merged" },
            report.mcp_servers_restored
        );
    }
    Ok(())
}

fn cmd_diff(config: &Config, remote: bool, from: Option<String>) -> Result<()> {
    let claude = paths::claude_dir()?;
    let staging = paths::staging_dir()?;

    let claude_json = if config.include_mcp_servers {
        paths::claude_json_file().ok()
    } else {
        None
    };
    let (entries, other_label) = if remote {
        let url = git::resolve_remote(None, config.remote.as_deref())?;
        let manifest = git::remote_manifest(&url, from.as_deref(), &config.effective_machine_id())?;
        let label = from
            .map(|m| format!("remote snapshot of {m:?}"))
            .unwrap_or_else(|| "remote snapshot".to_string());
        (
            diff::against_manifest(&claude, &staging, config, claude_json, &manifest)?,
            label,
        )
    } else {
        snapshot::require_staged(&staging)?;
        (
            diff::against_staged(&claude, &staging, config, claude_json)?,
            "staged snapshot".to_string(),
        )
    };
    if entries.is_empty() {
        println!("local {} matches the {other_label}", claude.display());
        return Ok(());
    }
    for e in &entries {
        let tag = match e.state {
            diff::DiffState::LocalOnly => "local only   ",
            diff::DiffState::OtherOnly => "snapshot only",
            diff::DiffState::Changed => "differs      ",
        };
        println!("  {tag} {}", e.rel);
    }
    println!(
        "{} difference(s) vs the {other_label}; `ccsync snapshot` refreshes staging, `ccsync restore` applies it",
        entries.len()
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
    mark_pulled(&staging, &format!("archive {}", file.display()))?;
    println!(
        "imported snapshot to {} — run `ccsync restore` to apply",
        staging.display()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pulled_marker_is_cleared_by_the_next_snapshot() {
        let tmp = tempfile::tempdir().unwrap();
        let claude = tmp.path().join("claude");
        std::fs::create_dir_all(&claude).unwrap();
        std::fs::write(claude.join("settings.json"), "{}").unwrap();
        std::env::set_var("HOME", tmp.path());
        let staging = tmp.path().join("staging");
        let build = |dry_run| {
            snapshot::build(
                &claude,
                &staging,
                &Config::default(),
                &SnapshotOptions {
                    dry_run,
                    allow_secrets: false,
                    claude_json: None,
                    profiles_root: None,
                },
            )
            .unwrap()
        };
        build(false);

        mark_pulled(&staging, "file:///remote at abc123").unwrap();
        assert_eq!(
            pulled_from(&staging).as_deref(),
            Some("file:///remote at abc123")
        );
        // A dry run (`status`) leaves staging, and so the marker, alone.
        build(true);
        assert!(pulled_from(&staging).is_some());
        build(false);
        assert_eq!(pulled_from(&staging), None);
    }
}
