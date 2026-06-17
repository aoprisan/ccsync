//! Background backup service.
//!
//! `ccsync daemon` runs a foreground loop that, on a fixed interval, builds a
//! sanitized snapshot of `~/.claude` and publishes it to the destination
//! configured in the `[service]` table — either the git `remote` or a
//! timestamped encrypted archive. It is pure orchestration over the existing
//! `snapshot`, `git`, and `archive` modules; no new transport logic lives here.
//!
//! `ccsync service install|uninstall|status` registers that loop with the
//! platform service manager (a systemd **user** unit on Linux, a launchd agent
//! on macOS) so it runs on login/boot. Each tick's archive lands under
//! `~/.config/ccsync/backups` by default, so `ccsync tui` lists them with no
//! extra wiring.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use chrono::{DateTime, Local};

use crate::archive;
use crate::config::{Config, ServiceConfig, ServiceDestination};
use crate::git;
use crate::paths;
use crate::snapshot::{self, SnapshotOptions};

/// Run a single snapshot+publish cycle and return a one-line human summary.
pub fn run_once(config: &Config) -> Result<String> {
    let claude = paths::claude_dir()?;
    let staging = paths::staging_dir()?;
    let opts = SnapshotOptions {
        dry_run: false,
        allow_secrets: config.service.allow_secrets,
    };
    let manifest = snapshot::build(&claude, &staging, config, &opts)?;
    let files = manifest.files.len();

    match config.service.destination {
        ServiceDestination::Git => {
            let remote = git::resolve_remote(None, config.remote.as_deref())?;
            git::push(&remote, &staging)?;
            Ok(format!("pushed {files} files to {remote}"))
        }
        ServiceDestination::Archive => {
            let pass = archive::passphrase_from_env()?;
            let dir = archive_dir(config)?;
            let out = dir.join(archive_filename(Local::now()));
            archive::create(&staging, &out, &pass)?;
            Ok(format!("wrote {files} files to {}", out.display()))
        }
    }
}

/// Run the background loop in the foreground until the process is terminated.
///
/// Per-tick errors are logged and swallowed so a transient failure (offline
/// remote, missing passphrase) never tears the daemon down — the service
/// manager keeps it alive and the next tick retries.
pub fn run_daemon(config: &Config) -> Result<()> {
    if !config.service.enabled {
        log("service.enabled is false in config.toml; nothing to do");
        log("set `enabled = true` under [service] to start automatic backups");
        return Ok(());
    }

    let minutes = config.service.interval_minutes.max(1);
    log(&format!(
        "daemon starting: every {minutes}m, destination = {}",
        destination_label(config.service.destination)
    ));

    loop {
        match run_once(config) {
            Ok(summary) => log(&format!("ok: {summary}")),
            Err(e) => log(&format!("error: {e:#}")),
        }
        std::thread::sleep(Duration::from_secs(minutes * 60));
    }
}

/// Resolve the directory for archive-destination backups: the configured
/// `service.backup_dir`, else ccsync's managed backups directory.
pub fn archive_dir(config: &Config) -> Result<PathBuf> {
    match &config.service.backup_dir {
        Some(dir) => Ok(dir.clone()),
        None => Ok(paths::backups_dir()?),
    }
}

/// Timestamped archive filename, e.g. `claude-backup-20260617-142530.tar.gz.age`.
///
/// The `.tar.gz.age` suffix matches the convention `archive::create` produces
/// and the `.age` filter `backups::collect_archives` lists, so daemon-written
/// archives appear in the TUI automatically.
pub fn archive_filename(now: DateTime<Local>) -> String {
    format!("claude-backup-{}.tar.gz.age", now.format("%Y%m%d-%H%M%S"))
}

fn destination_label(dest: ServiceDestination) -> &'static str {
    match dest {
        ServiceDestination::Git => "git",
        ServiceDestination::Archive => "archive",
    }
}

/// Timestamped log line to stdout, captured by journald / launchd logs.
fn log(msg: &str) {
    println!("[ccsync {}] {msg}", Local::now().format("%Y-%m-%dT%H:%M:%S"));
}

/// Print the destination-specific reminder about secrets the service manager
/// will not inherit from an interactive shell.
fn warn_about_secrets(service: &ServiceConfig) {
    match service.destination {
        ServiceDestination::Archive => {
            println!(
                "note: archive backups need CCSYNC_PASSPHRASE, which the service does NOT\n      \
                 inherit from your shell. Add it to the unit (see below) before relying on it."
            );
        }
        ServiceDestination::Git => {
            println!(
                "note: git push relies on your SSH keys / credential helper, which a background\n      \
                 service may not have. Prefer an HTTPS remote with a stored credential, or ensure\n      \
                 the agent can reach your key."
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Linux: systemd user unit
// ---------------------------------------------------------------------------

/// Render the systemd user unit that runs `ccsync daemon`.
#[cfg(target_os = "linux")]
pub fn systemd_unit(exec_path: &Path) -> String {
    format!(
        "[Unit]\n\
         Description=ccsync background backup of ~/.claude\n\
         After=network-online.target\n\
         \n\
         [Service]\n\
         Type=simple\n\
         ExecStart={exec} daemon\n\
         Restart=on-failure\n\
         RestartSec=30\n\
         \n\
         [Install]\n\
         WantedBy=default.target\n",
        exec = exec_path.display()
    )
}

#[cfg(target_os = "linux")]
fn systemd_unit_path() -> Result<PathBuf> {
    let base = dirs::config_dir().context("locating ~/.config")?;
    Ok(base.join("systemd").join("user").join("ccsync.service"))
}

#[cfg(target_os = "linux")]
pub fn install(config: &Config) -> Result<()> {
    let exe = std::env::current_exe().context("resolving the ccsync executable path")?;
    let unit_path = systemd_unit_path()?;
    if let Some(parent) = unit_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&unit_path, systemd_unit(&exe))
        .with_context(|| format!("writing {}", unit_path.display()))?;
    println!("wrote systemd unit to {}", unit_path.display());

    let _ = run_tool("systemctl", &["--user", "daemon-reload"]);
    match run_tool("systemctl", &["--user", "enable", "--now", "ccsync.service"]) {
        Ok(_) => println!("enabled and started ccsync.service"),
        Err(e) => {
            println!("could not enable the service automatically ({e})");
            println!("enable it manually with: systemctl --user enable --now ccsync.service");
            println!("(headless? you may need: loginctl enable-linger $USER)");
        }
    }
    warn_about_secrets(&config.service);
    Ok(())
}

#[cfg(target_os = "linux")]
pub fn uninstall() -> Result<()> {
    let _ = run_tool("systemctl", &["--user", "disable", "--now", "ccsync.service"]);
    let unit_path = systemd_unit_path()?;
    if unit_path.exists() {
        std::fs::remove_file(&unit_path)
            .with_context(|| format!("removing {}", unit_path.display()))?;
        println!("removed {}", unit_path.display());
    } else {
        println!("no systemd unit found at {}", unit_path.display());
    }
    let _ = run_tool("systemctl", &["--user", "daemon-reload"]);
    Ok(())
}

#[cfg(target_os = "linux")]
pub fn status() -> Result<()> {
    let unit_path = systemd_unit_path()?;
    println!(
        "unit: {} ({})",
        unit_path.display(),
        if unit_path.exists() { "installed" } else { "not installed" }
    );
    match run_tool("systemctl", &["--user", "--no-pager", "status", "ccsync.service"]) {
        Ok(out) => print!("{out}"),
        Err(e) => println!("systemctl status unavailable ({e})"),
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// macOS: launchd agent
// ---------------------------------------------------------------------------

/// Render the launchd agent plist that runs `ccsync daemon`.
#[cfg(target_os = "macos")]
pub fn launchd_plist(exec_path: &Path) -> String {
    let home = dirs::home_dir().map(|h| h.display().to_string()).unwrap_or_default();
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \
         \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
         <plist version=\"1.0\">\n\
         <dict>\n\
         \t<key>Label</key>\n\t<string>com.ccsync.daemon</string>\n\
         \t<key>ProgramArguments</key>\n\t<array>\n\
         \t\t<string>{exec}</string>\n\t\t<string>daemon</string>\n\t</array>\n\
         \t<key>RunAtLoad</key>\n\t<true/>\n\
         \t<key>KeepAlive</key>\n\t<true/>\n\
         \t<key>StandardOutPath</key>\n\t<string>{home}/Library/Logs/ccsync.log</string>\n\
         \t<key>StandardErrorPath</key>\n\t<string>{home}/Library/Logs/ccsync.log</string>\n\
         </dict>\n\
         </plist>\n",
        exec = exec_path.display(),
    )
}

#[cfg(target_os = "macos")]
fn launchd_plist_path() -> Result<PathBuf> {
    let home = dirs::home_dir().context("locating the home directory")?;
    Ok(home
        .join("Library")
        .join("LaunchAgents")
        .join("com.ccsync.daemon.plist"))
}

#[cfg(target_os = "macos")]
pub fn install(config: &Config) -> Result<()> {
    let exe = std::env::current_exe().context("resolving the ccsync executable path")?;
    let plist_path = launchd_plist_path()?;
    if let Some(parent) = plist_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&plist_path, launchd_plist(&exe))
        .with_context(|| format!("writing {}", plist_path.display()))?;
    println!("wrote launchd agent to {}", plist_path.display());

    // Unload any prior copy first so a re-install picks up changes.
    let _ = run_tool("launchctl", &["unload", &plist_path.to_string_lossy()]);
    match run_tool("launchctl", &["load", "-w", &plist_path.to_string_lossy()]) {
        Ok(_) => println!("loaded com.ccsync.daemon"),
        Err(e) => {
            println!("could not load the agent automatically ({e})");
            println!(
                "load it manually with: launchctl load -w {}",
                plist_path.display()
            );
        }
    }
    warn_about_secrets(&config.service);
    Ok(())
}

#[cfg(target_os = "macos")]
pub fn uninstall() -> Result<()> {
    let plist_path = launchd_plist_path()?;
    let _ = run_tool("launchctl", &["unload", &plist_path.to_string_lossy()]);
    if plist_path.exists() {
        std::fs::remove_file(&plist_path)
            .with_context(|| format!("removing {}", plist_path.display()))?;
        println!("removed {}", plist_path.display());
    } else {
        println!("no launchd agent found at {}", plist_path.display());
    }
    Ok(())
}

#[cfg(target_os = "macos")]
pub fn status() -> Result<()> {
    let plist_path = launchd_plist_path()?;
    println!(
        "agent: {} ({})",
        plist_path.display(),
        if plist_path.exists() { "installed" } else { "not installed" }
    );
    match run_tool("launchctl", &["list", "com.ccsync.daemon"]) {
        Ok(out) => print!("{out}"),
        Err(e) => println!("launchctl status unavailable ({e})"),
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Other platforms
// ---------------------------------------------------------------------------

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub fn install(_config: &Config) -> Result<()> {
    Err(crate::error::CcError::ServiceUnsupportedPlatform.into())
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub fn uninstall() -> Result<()> {
    Err(crate::error::CcError::ServiceUnsupportedPlatform.into())
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub fn status() -> Result<()> {
    Err(crate::error::CcError::ServiceUnsupportedPlatform.into())
}

/// Run a service-manager command, returning its stdout or an error carrying
/// stderr. Used best-effort by install/uninstall/status.
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn run_tool(bin: &str, args: &[&str]) -> Result<String> {
    use anyhow::anyhow;
    let out = std::process::Command::new(bin)
        .args(args)
        .output()
        .with_context(|| format!("running `{bin} {}`", args.join(" ")))?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        return Err(anyhow!("{bin} {}: {}", args.join(" "), stderr.trim()));
    }
    Ok(String::from_utf8_lossy(&out.stdout).to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn with_config_dir<T>(dir: &std::path::Path, f: impl FnOnce() -> T) -> T {
        let prev = std::env::var("XDG_CONFIG_HOME").ok();
        std::env::set_var("XDG_CONFIG_HOME", dir);
        let out = f();
        match prev {
            Some(p) => std::env::set_var("XDG_CONFIG_HOME", p),
            None => std::env::remove_var("XDG_CONFIG_HOME"),
        }
        out
    }

    #[test]
    fn archive_filename_uses_age_suffix_and_timestamp() {
        let now = Local
            .with_ymd_and_hms(2026, 6, 17, 14, 25, 30)
            .single()
            .unwrap();
        let name = archive_filename(now);
        assert_eq!(name, "claude-backup-20260617-142530.tar.gz.age");
        // `.age` is what backups::collect_archives filters on.
        assert!(name.ends_with(".age"));
    }

    #[test]
    fn archive_dir_defaults_to_backups_dir() {
        let tmp = tempfile::tempdir().unwrap();
        with_config_dir(tmp.path(), || {
            let config = Config::default();
            assert_eq!(archive_dir(&config).unwrap(), paths::backups_dir().unwrap());
        });
    }

    #[test]
    fn archive_dir_honors_explicit_backup_dir() {
        let mut config = Config::default();
        config.service.backup_dir = Some(PathBuf::from("/mnt/ext/backups"));
        assert_eq!(archive_dir(&config).unwrap(), PathBuf::from("/mnt/ext/backups"));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn systemd_unit_runs_daemon() {
        let unit = systemd_unit(Path::new("/usr/local/bin/ccsync"));
        assert!(unit.contains("ExecStart=/usr/local/bin/ccsync daemon"));
        assert!(unit.contains("WantedBy=default.target"));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn launchd_plist_runs_daemon() {
        let plist = launchd_plist(Path::new("/usr/local/bin/ccsync"));
        assert!(plist.contains("<string>/usr/local/bin/ccsync</string>"));
        assert!(plist.contains("<string>daemon</string>"));
        assert!(plist.contains("com.ccsync.daemon"));
    }
}