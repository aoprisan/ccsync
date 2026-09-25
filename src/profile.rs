//! Named profiles: work/personal/client-X overlays over the shared `~/.claude`
//! base state.
//!
//! A profile *owns* a configurable set of top-level components (by default
//! `settings.json`, `CLAUDE.md`, `agents/`, `skills/`, `commands/`,
//! `output-styles/`) plus, optionally, the user-scope `mcpServers` of
//! `~/.claude.json`. Everything else — sessions, agent memory, keybindings,
//! todos — is shared base state that a switch never touches.
//!
//! Owned components are **swapped wholesale** on switch rather than merged:
//! the outgoing profile's live state is first captured back into its store
//! (so edits made while it was active are never lost), then the incoming
//! profile's components replace them. Merging would let the previous
//! profile's settings keys and skills bleed into the next one as ghosts.
//!
//! Store layout (under `<config>/ccsync/profiles/`):
//! ```text
//! profiles/
//! ├── active.json              # {"name","previous","switched_at","backup_dir"}
//! └── work/
//!     ├── profile.toml         # description, optional component override
//!     ├── data/                # same layout as a snapshot's data/
//!     └── mcp-servers.json     # user-scope mcpServers owned by this profile
//! ```
//!
//! The store rides inside snapshots under the reserved `ccsync-profiles/`
//! component (see `snapshot`/`restore`), so profiles sync across machines
//! through the normal push/pull/export flow. `active.json` is machine-local
//! and never synced.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Component, Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use walkdir::WalkDir;

use crate::config::Config;
use crate::diff::{self, DiffEntry, DiffState};
use crate::mcp;
use crate::redact;
use crate::restore;

/// Reserved top-level name under which the profile store is carried inside a
/// snapshot's `data/` tree. Restore routes it back into the local store.
pub const PROFILES_COMPONENT: &str = "ccsync-profiles";

/// Machine-local pointer to the active profile, doubling as the switch
/// journal: `previous` and `backup_dir` are what `profile rollback` uses.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActiveProfile {
    pub name: String,
    pub previous: Option<String>,
    pub switched_at: String,
    pub backup_dir: Option<PathBuf>,
}

/// Per-profile metadata stored as `profile.toml`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ProfileMeta {
    pub description: String,
    /// Overrides `config.profiles.components` for this profile when set.
    pub components: Option<Vec<String>>,
}

/// The live state a profile captures from / applies to.
pub struct LiveState<'a> {
    pub claude_dir: &'a Path,
    /// `~/.claude.json`, when user-scope MCP ownership is in play.
    pub claude_json: Option<&'a Path>,
}

#[derive(Debug)]
pub struct SwitchReport {
    pub from: Option<String>,
    pub to: String,
    /// Files captured back into the outgoing profile's store.
    pub captured_files: usize,
    /// Files applied from the incoming profile's store.
    pub applied_files: usize,
    /// User-scope MCP servers installed for the incoming profile.
    pub mcp_servers: usize,
    pub backup_dir: Option<PathBuf>,
}

pub fn profile_dir(root: &Path, name: &str) -> PathBuf {
    root.join(name)
}

fn data_dir(root: &Path, name: &str) -> PathBuf {
    profile_dir(root, name).join("data")
}

fn meta_path(root: &Path, name: &str) -> PathBuf {
    profile_dir(root, name).join("profile.toml")
}

fn mcp_path(root: &Path, name: &str) -> PathBuf {
    profile_dir(root, name).join(mcp::MCP_FILE)
}

fn active_path(root: &Path) -> PathBuf {
    root.join("active.json")
}

/// Profile names become directory names and travel inside snapshots; keep
/// them boring so they can never escape the store or collide with the
/// `active.json` marker.
fn validate_name(name: &str) -> Result<()> {
    let ok = !name.is_empty()
        && !name.starts_with('.')
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'));
    if !ok || name == "active.json" {
        bail!("invalid profile name {name:?}: use letters, digits, '-', '_'");
    }
    Ok(())
}

pub fn exists(root: &Path, name: &str) -> bool {
    validate_name(name).is_ok() && profile_dir(root, name).is_dir()
}

/// All profile names in the store, sorted.
pub fn list(root: &Path) -> Result<Vec<String>> {
    let mut names = Vec::new();
    if !root.is_dir() {
        return Ok(names);
    }
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        if let Some(name) = entry.file_name().to_str() {
            if validate_name(name).is_ok() {
                names.push(name.to_string());
            }
        }
    }
    names.sort();
    Ok(names)
}

pub fn active(root: &Path) -> Result<Option<ActiveProfile>> {
    let path = active_path(root);
    if !path.exists() {
        return Ok(None);
    }
    let text = fs::read_to_string(&path)?;
    Ok(Some(
        serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))?,
    ))
}

fn write_active(root: &Path, state: Option<&ActiveProfile>) -> Result<()> {
    let path = active_path(root);
    match state {
        Some(s) => {
            fs::create_dir_all(root)?;
            fs::write(&path, serde_json::to_string_pretty(s)?)?;
        }
        None => {
            let _ = fs::remove_file(&path);
        }
    }
    Ok(())
}

pub fn read_meta(root: &Path, name: &str) -> Result<ProfileMeta> {
    let path = meta_path(root, name);
    if !path.exists() {
        return Ok(ProfileMeta::default());
    }
    let meta: ProfileMeta = toml::from_str(&fs::read_to_string(&path)?)
        .with_context(|| format!("parsing {}", path.display()))?;
    // `profile.toml` rides inside snapshots, so it is remote input: refuse a
    // component list that could reach outside the owned set on load.
    if let Some(comps) = &meta.components {
        validate_components(comps).with_context(|| format!("invalid {}", path.display()))?;
    }
    Ok(meta)
}

/// Top-level entries that are shared base state (or ccsync-internal) and must
/// never be owned -- and thus wholesale swapped/deleted -- by a profile.
const RESERVED_COMPONENTS: &[&str] = &["projects", "todos", PROFILES_COMPONENT];

/// Every owned component must be a single plain top-level name under the
/// claude dir: no separators, `..`, absolute paths, credential files, or
/// shared base state. Owned components are removed and replaced wholesale, so
/// anything else would let a (synced, untrusted) `profile.toml` delete or
/// overwrite arbitrary files.
fn validate_component(comp: &str) -> Result<()> {
    let mut parts = Path::new(comp).components();
    let single = match (parts.next(), parts.next()) {
        (Some(Component::Normal(n)), None) => n.to_str() == Some(comp),
        _ => false,
    };
    if !single || comp.contains(['/', '\\']) {
        bail!("invalid profile component {comp:?}: must be a single top-level name");
    }
    if RESERVED_COMPONENTS
        .iter()
        .any(|r| r.eq_ignore_ascii_case(comp))
    {
        bail!("invalid profile component {comp:?}: shared base state cannot be owned by a profile");
    }
    if redact::is_credential_file(comp) {
        bail!("invalid profile component {comp:?}: credential files cannot be owned by a profile");
    }
    Ok(())
}

fn validate_components(comps: &[String]) -> Result<()> {
    comps.iter().try_for_each(|c| validate_component(c))
}

/// The component set this profile owns (validated).
pub fn components(root: &Path, name: &str, config: &Config) -> Result<Vec<String>> {
    let comps = read_meta(root, name)?
        .components
        .unwrap_or_else(|| config.profiles.components.clone());
    validate_components(&comps).with_context(|| format!("profile {name:?}"))?;
    Ok(comps)
}

/// Create an empty profile, optionally capturing the current live state into
/// it right away.
pub fn create(
    root: &Path,
    name: &str,
    description: Option<String>,
    config: &Config,
    from_current: Option<&LiveState>,
) -> Result<usize> {
    validate_name(name)?;
    let dir = profile_dir(root, name);
    if dir.exists() {
        bail!("profile {name:?} already exists at {}", dir.display());
    }
    fs::create_dir_all(data_dir(root, name))?;
    let meta = ProfileMeta {
        description: description.unwrap_or_default(),
        components: None,
    };
    fs::write(meta_path(root, name), toml::to_string_pretty(&meta)?)?;
    match from_current {
        Some(live) => capture_into(root, name, live, config),
        None => Ok(0),
    }
}

/// Capture the live state of every owned component into the profile's store,
/// replacing what the store held. This is the "capture-back" step of a
/// switch: edits made while a profile was active always land in its store.
pub fn capture_into(root: &Path, name: &str, live: &LiveState, config: &Config) -> Result<usize> {
    validate_name(name)?;
    if !profile_dir(root, name).is_dir() {
        bail!("profile {name:?} does not exist; create it first");
    }
    let data = data_dir(root, name);
    let mut captured = 0;
    for comp in components(root, name, config)? {
        let src = live.claude_dir.join(&comp);
        let dst = data.join(&comp);
        remove_path(&dst)?;
        if entry_exists(&src) {
            captured += copy_path(&src, &dst)?;
        }
    }

    // User-scope MCP servers.
    if config.profiles.include_user_mcp {
        let store = mcp_path(root, name);
        remove_path(&store)?;
        if let Some(cj) = live.claude_json {
            if let Some(doc) = mcp::extract(cj)? {
                if let Some(servers) = doc.get("mcpServers") {
                    let user_only = serde_json::json!({ "mcpServers": servers });
                    fs::write(&store, serde_json::to_string_pretty(&user_only)?)?;
                }
            }
        }
    }
    Ok(captured)
}

/// Switch the live state to `name`:
/// 1. capture the currently-active profile back into its store,
/// 2. surface any new hook commands the target would install,
/// 3. back up the affected live components,
/// 4. journal the switch (`active.json`),
/// 5. swap components in and replace user-scope MCP servers.
///
/// Any error during step 5 restores the backup and re-journals the previous
/// profile, so a failed switch never leaves half-applied state.
pub fn switch(
    root: &Path,
    name: &str,
    live: &LiveState,
    config: &Config,
    confirm_hooks: bool,
) -> Result<SwitchReport> {
    validate_name(name)?;
    if !profile_dir(root, name).is_dir() {
        let known = list(root)?.join(", ");
        bail!("profile {name:?} does not exist (have: {known}); `ccsync profile create {name}`");
    }
    let prev = active(root)?;

    // 1. Capture-back.
    let mut captured_files = 0;
    if let Some(prev) = &prev {
        if exists(root, &prev.name) {
            captured_files = capture_into(root, &prev.name, live, config)?;
        }
    }
    if prev.as_ref().map(|p| p.name.as_str()) == Some(name) {
        // Switching to the already-active profile = sync its store.
        return Ok(SwitchReport {
            from: Some(name.to_string()),
            to: name.to_string(),
            captured_files,
            applied_files: 0,
            mcp_servers: 0,
            backup_dir: None,
        });
    }

    // 2. Hooks gate, before anything is written.
    if confirm_hooks {
        let incoming = restore::hook_commands_in(&data_dir(root, name).join("settings.json"))?;
        let existing = restore::hook_commands_in(&live.claude_dir.join("settings.json"))?;
        let new_hooks: BTreeSet<String> = incoming.difference(&existing).cloned().collect();
        if !new_hooks.is_empty() {
            restore::confirm_hook_install(&new_hooks)?;
        }
    }

    let comps = components(root, name, config)?;

    // 3. Backup the live components this switch will replace.
    let backup_dir = backup_components(live, &comps, config)?;

    // 4. Journal before mutating.
    let journal = ActiveProfile {
        name: name.to_string(),
        previous: prev.as_ref().map(|p| p.name.clone()),
        switched_at: chrono::Utc::now().to_rfc3339(),
        backup_dir: backup_dir.clone(),
    };
    write_active(root, Some(&journal))?;

    // 5. Apply, rolling back to the backup on any failure.
    match apply_profile(root, name, live, config, &comps) {
        Ok((applied_files, mcp_servers)) => Ok(SwitchReport {
            from: prev.map(|p| p.name),
            to: name.to_string(),
            captured_files,
            applied_files,
            mcp_servers,
            backup_dir,
        }),
        Err(e) => {
            let restored = restore_backup(live, backup_dir.as_deref(), &comps, config);
            write_active(root, prev.as_ref())?;
            match restored {
                Ok(()) => Err(e.context(format!(
                    "switching to {name:?} failed; previous state restored from backup"
                ))),
                Err(re) => Err(e.context(format!(
                    "switching to {name:?} failed AND restoring the backup also failed ({re:#}); \
                     recover manually from {}",
                    backup_dir
                        .as_deref()
                        .map(|p| p.display().to_string())
                        .unwrap_or_else(|| "<no backup>".into())
                ))),
            }
        }
    }
}

/// Revert the last switch: switch back to `previous`, or — when the journal
/// has no previous profile — capture the current profile back into its store,
/// restore the pre-switch backup, and clear the active marker.
///
/// `confirm_hooks` gates hooks exactly like `switch`: the previous profile's
/// store may have been replaced by a pull/restore since it was last active.
pub fn rollback(
    root: &Path,
    live: &LiveState,
    config: &Config,
    confirm_hooks: bool,
) -> Result<String> {
    let Some(current) = active(root)? else {
        bail!("no active profile to roll back from");
    };
    if let Some(previous) = &current.previous {
        switch(root, previous, live, config, confirm_hooks)?;
        return Ok(format!("switched back to {previous:?}"));
    }
    let comps = if exists(root, &current.name) {
        // Capture-back first so edits made while `current` was active are
        // not lost when the backup replaces them.
        capture_into(root, &current.name, live, config)?;
        components(root, &current.name, config)?
    } else {
        let comps = config.profiles.components.clone();
        validate_components(&comps)?;
        comps
    };
    restore_backup(live, current.backup_dir.as_deref(), &comps, config)?;
    write_active(root, None)?;
    Ok(format!(
        "restored pre-switch state{}",
        current
            .backup_dir
            .map(|b| format!(" from {}", b.display()))
            .unwrap_or_default()
    ))
}

pub fn delete(root: &Path, name: &str) -> Result<()> {
    validate_name(name)?;
    if let Some(a) = active(root)? {
        if a.name == name {
            bail!("profile {name:?} is active; switch away before deleting it");
        }
    }
    let dir = profile_dir(root, name);
    if !dir.is_dir() {
        bail!("profile {name:?} does not exist");
    }
    fs::remove_dir_all(&dir).with_context(|| format!("deleting {}", dir.display()))?;
    Ok(())
}

/// Per-component diff between the live state and the profile's store.
/// `LocalOnly` = present live but not in the store.
pub fn diff_live(
    root: &Path,
    name: &str,
    live: &LiveState,
    config: &Config,
) -> Result<Vec<DiffEntry>> {
    validate_name(name)?;
    if !profile_dir(root, name).is_dir() {
        bail!("profile {name:?} does not exist");
    }
    let data = data_dir(root, name);
    let mut out = Vec::new();
    for comp in components(root, name, config)? {
        let local = live.claude_dir.join(&comp);
        let store = data.join(&comp);
        if local.is_file() || store.is_file() {
            let l = local.is_file().then(|| fs::read(&local)).transpose()?;
            let s = store.is_file().then(|| fs::read(&store)).transpose()?;
            let state = match (l, s) {
                (Some(l), Some(s)) if l == s => continue,
                (Some(_), Some(_)) => DiffState::Changed,
                (Some(_), None) => DiffState::LocalOnly,
                (None, Some(_)) => DiffState::OtherOnly,
                (None, None) => continue,
            };
            out.push(DiffEntry { rel: comp, state });
        } else {
            for entry in diff::diff_trees(&local, &store)? {
                out.push(DiffEntry {
                    rel: format!("{comp}/{}", entry.rel),
                    state: entry.state,
                });
            }
        }
    }
    Ok(out)
}

/// Number of user-scope MCP servers stored for a profile.
pub fn stored_mcp_count(root: &Path, name: &str) -> usize {
    let path = mcp_path(root, name);
    let Ok(text) = fs::read_to_string(path) else {
        return 0;
    };
    serde_json::from_str::<serde_json::Value>(&text)
        .map(|doc| mcp::server_count(&doc))
        .unwrap_or(0)
}

fn apply_profile(
    root: &Path,
    name: &str,
    live: &LiveState,
    config: &Config,
    comps: &[String],
) -> Result<(usize, usize)> {
    let data = data_dir(root, name);
    let mut applied = 0;
    for comp in comps {
        let src = data.join(comp);
        let dst = live.claude_dir.join(comp);
        // Owned components are swapped wholesale: absent in the store means
        // absent live (the outgoing state was captured back already).
        remove_path(&dst)?;
        if entry_exists(&src) {
            applied += copy_path(&src, &dst)?;
        }
    }

    let mut mcp_servers = 0;
    if config.profiles.include_user_mcp {
        if let Some(cj) = live.claude_json {
            let store = mcp_path(root, name);
            let servers = if store.exists() {
                let doc: serde_json::Value = serde_json::from_str(&fs::read_to_string(&store)?)
                    .with_context(|| format!("parsing {}", store.display()))?;
                doc.get("mcpServers").cloned()
            } else {
                None
            };
            mcp_servers = mcp::replace_user_scope(cj, servers.as_ref())?;
        }
    }
    Ok((applied, mcp_servers))
}

/// Copy the live components (and `~/.claude.json`) into a timestamped backup
/// directory next to the claude dir. Returns `None` when nothing existed to
/// back up.
fn backup_components(
    live: &LiveState,
    comps: &[String],
    config: &Config,
) -> Result<Option<PathBuf>> {
    let ts = chrono::Utc::now().format("%Y%m%d-%H%M%S");
    let dir_name = live
        .claude_dir
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| ".claude".into());
    let base = format!("{dir_name}.ccsync-profile-backup-{ts}");
    let backup = claim_backup_dir(live.claude_dir, &base)?;

    let result = (|| -> Result<bool> {
        let mut any = false;
        for comp in comps {
            let src = live.claude_dir.join(comp);
            if entry_exists(&src) {
                copy_path(&src, &backup.join(comp))?;
                any = true;
            }
        }
        if config.profiles.include_user_mcp {
            if let Some(cj) = live.claude_json {
                if cj.exists() {
                    fs::copy(cj, backup.join("claude.json.bak"))?;
                    any = true;
                }
            }
        }
        Ok(any)
    })();
    match result {
        Ok(true) => Ok(Some(backup)),
        Ok(false) => {
            let _ = fs::remove_dir(&backup);
            Ok(None)
        }
        Err(e) => Err(e.context(format!("backing up into {}", backup.display()))),
    }
}

/// Atomically claim a fresh backup directory next to `claude_dir`. Switches
/// within the same second get a numeric suffix instead of layering into one
/// directory.
fn claim_backup_dir(claude_dir: &Path, base: &str) -> Result<PathBuf> {
    for n in 0u32..10_000 {
        let name = if n == 0 {
            base.to_string()
        } else {
            format!("{base}-{n}")
        };
        let candidate = claude_dir.with_file_name(name);
        match fs::create_dir(&candidate) {
            Ok(()) => return Ok(candidate),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => {
                return Err(e).with_context(|| format!("creating {}", candidate.display()));
            }
        }
    }
    bail!("could not claim a unique backup directory for {base}")
}

/// Put the backed-up components back, plus the user-scope `mcpServers` of
/// `~/.claude.json`. The rest of `~/.claude.json` (OAuth tokens refreshed
/// since the backup, trust decisions, history) is left live, never
/// overwritten with the stale copy.
fn restore_backup(
    live: &LiveState,
    backup: Option<&Path>,
    comps: &[String],
    config: &Config,
) -> Result<()> {
    for comp in comps {
        let dst = live.claude_dir.join(comp);
        remove_path(&dst)?;
        if let Some(backup) = backup {
            let src = backup.join(comp);
            if entry_exists(&src) {
                copy_path(&src, &dst)?;
            }
        }
    }
    if config.profiles.include_user_mcp {
        if let Some(cj) = live.claude_json {
            let saved = backup.map(|b| b.join("claude.json.bak"));
            let servers = match saved.filter(|p| p.exists()) {
                Some(saved) => {
                    let doc: serde_json::Value = serde_json::from_str(&fs::read_to_string(&saved)?)
                        .with_context(|| format!("parsing {}", saved.display()))?;
                    doc.get("mcpServers").cloned()
                }
                // No ~/.claude.json existed before the switch: no user-scope
                // servers to restore.
                None => None,
            };
            if cj.exists() || servers.is_some() {
                mcp::replace_user_scope(cj, servers.as_ref())?;
            }
        }
    }
    Ok(())
}

/// Copy a file, symlink, or directory tree, hard-blocking credential files
/// (they must never enter a profile store, which can sync).
///
/// Symlinks are preserved as symlinks (never followed), so a dotfile-managed
/// `skills/foo -> ~/dotfiles/...` link survives a capture/apply round trip.
/// `dst` is removed first and every entry is created fresh, so nothing is
/// ever written *through* a pre-existing symlink at the destination.
fn copy_path(src: &Path, dst: &Path) -> Result<usize> {
    let meta = fs::symlink_metadata(src).with_context(|| format!("reading {}", src.display()))?;
    if !meta.is_dir() {
        check_credential(src)?;
        if let Some(parent) = dst.parent() {
            fs::create_dir_all(parent)?;
        }
        remove_path(dst)?;
        return Ok(usize::from(copy_entry(src, dst, &meta)?));
    }
    remove_path(dst)?;
    let mut copied = 0;
    for entry in WalkDir::new(src).follow_links(false) {
        let entry = entry?;
        let rel = entry.path().strip_prefix(src).expect("under src");
        let target = dst.join(rel);
        if entry.file_type().is_dir() {
            fs::create_dir_all(&target)
                .with_context(|| format!("creating {}", target.display()))?;
            continue;
        }
        check_credential(entry.path())?;
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent)?;
        }
        let meta = fs::symlink_metadata(entry.path())?;
        if copy_entry(entry.path(), &target, &meta)? {
            copied += 1;
        }
    }
    Ok(copied)
}

/// Copy one non-directory entry into a fresh `dst`. Symlinks are recreated
/// (not followed); special files (sockets, fifos) are skipped (`false`).
fn copy_entry(src: &Path, dst: &Path, meta: &fs::Metadata) -> Result<bool> {
    if meta.file_type().is_symlink() {
        let target =
            fs::read_link(src).with_context(|| format!("reading link {}", src.display()))?;
        return make_symlink(&target, dst)
            .with_context(|| format!("recreating link {}", dst.display()));
    }
    if !meta.is_file() {
        return Ok(false);
    }
    fs::copy(src, dst).with_context(|| format!("copying {}", src.display()))?;
    Ok(true)
}

#[cfg(unix)]
fn make_symlink(target: &Path, link: &Path) -> std::io::Result<bool> {
    std::os::unix::fs::symlink(target, link)?;
    Ok(true)
}

/// Symlinks are not preserved off unix; skip rather than follow them.
#[cfg(not(unix))]
fn make_symlink(_target: &Path, _link: &Path) -> std::io::Result<bool> {
    Ok(false)
}

/// `exists()` without following symlinks (a dangling link still exists).
fn entry_exists(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok()
}

fn check_credential(path: &Path) -> Result<()> {
    let name = path
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default();
    if redact::is_credential_file(&name) {
        return Err(anyhow!(
            "refusing to copy credential file into a profile: {}",
            path.display()
        ));
    }
    Ok(())
}

/// Remove a file, directory tree, or symlink. Never follows a symlink: a link
/// to a directory removes only the link, and a dangling link is removed too.
fn remove_path(path: &Path) -> Result<()> {
    let Ok(meta) = fs::symlink_metadata(path) else {
        return Ok(());
    };
    if meta.is_dir() {
        fs::remove_dir_all(path).with_context(|| format!("removing {}", path.display()))?;
    } else {
        fs::remove_file(path).with_context(|| format!("removing {}", path.display()))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(path: &Path, content: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
    }

    struct Fixture {
        _tmp: tempfile::TempDir,
        root: PathBuf,
        claude: PathBuf,
        claude_json: PathBuf,
    }

    impl Fixture {
        fn new() -> Self {
            let tmp = tempfile::tempdir().unwrap();
            let claude = tmp.path().join("claude");
            let claude_json = tmp.path().join(".claude.json");
            write(&claude.join("settings.json"), r#"{"theme":"dark"}"#);
            write(&claude.join("skills/review/SKILL.md"), "# review");
            write(&claude.join("projects/-home-x-p/s.jsonl"), "{}\n");
            write(
                &claude_json,
                r#"{"oauthAccount":{"t":"keep"},"mcpServers":{"fetch":{"command":"uvx"}}}"#,
            );
            Fixture {
                root: tmp.path().join("profiles"),
                claude,
                claude_json,
                _tmp: tmp,
            }
        }

        fn live(&self) -> LiveState<'_> {
            LiveState {
                claude_dir: &self.claude,
                claude_json: Some(&self.claude_json),
            }
        }
    }

    #[test]
    fn create_from_current_and_list() {
        let f = Fixture::new();
        let cfg = Config::default();
        let n = create(
            &f.root,
            "work",
            Some("day job".into()),
            &cfg,
            Some(&f.live()),
        )
        .unwrap();
        assert!(n >= 2, "captured settings + skill, got {n}");
        assert_eq!(list(&f.root).unwrap(), vec!["work"]);

        // Store holds the captured components and the user-scope MCP servers.
        assert!(data_dir(&f.root, "work").join("settings.json").exists());
        assert!(data_dir(&f.root, "work")
            .join("skills/review/SKILL.md")
            .exists());
        let mcp_doc: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(mcp_path(&f.root, "work")).unwrap()).unwrap();
        assert_eq!(mcp_doc["mcpServers"]["fetch"]["command"], "uvx");
        // OAuth state never enters the store.
        assert!(mcp_doc.get("oauthAccount").is_none());

        // Duplicate create is refused; bad names are refused.
        assert!(create(&f.root, "work", None, &cfg, None).is_err());
        assert!(create(&f.root, "../evil", None, &cfg, None).is_err());
        assert!(create(&f.root, "", None, &cfg, None).is_err());
    }

    #[test]
    fn switch_swaps_components_and_mcp_and_preserves_base() {
        let f = Fixture::new();
        let cfg = Config::default();
        create(&f.root, "work", None, &cfg, Some(&f.live())).unwrap();
        // Activate work so capture-back has an owner.
        switch(&f.root, "work", &f.live(), &cfg, false).unwrap();

        // Build a distinct personal profile.
        create(&f.root, "personal", None, &cfg, None).unwrap();
        write(
            &data_dir(&f.root, "personal").join("settings.json"),
            r#"{"theme":"light"}"#,
        );
        write(
            &data_dir(&f.root, "personal").join("skills/cook/SKILL.md"),
            "# cook",
        );
        write(
            &mcp_path(&f.root, "personal"),
            r#"{"mcpServers":{"home":{"command":"home-mcp"}}}"#,
        );

        // Mutate live state while `work` is active, then switch.
        write(&f.claude.join("settings.json"), r#"{"theme":"midnight"}"#);
        let report = switch(&f.root, "personal", &f.live(), &cfg, false).unwrap();
        assert_eq!(report.from.as_deref(), Some("work"));

        // Live state now shows personal's components.
        let settings = fs::read_to_string(f.claude.join("settings.json")).unwrap();
        assert!(settings.contains("light"));
        assert!(f.claude.join("skills/cook/SKILL.md").exists());
        assert!(!f.claude.join("skills/review/SKILL.md").exists());
        // Shared base state untouched.
        assert!(f.claude.join("projects/-home-x-p/s.jsonl").exists());
        // User-scope MCP replaced, other ~/.claude.json keys intact.
        let cj: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&f.claude_json).unwrap()).unwrap();
        assert_eq!(cj["mcpServers"]["home"]["command"], "home-mcp");
        assert!(cj["mcpServers"].get("fetch").is_none());
        assert_eq!(cj["oauthAccount"]["t"], "keep");

        // The mid-session edit was captured back into work's store.
        let captured = fs::read_to_string(data_dir(&f.root, "work").join("settings.json")).unwrap();
        assert!(captured.contains("midnight"));

        // Round-trip back to work restores everything.
        let report = switch(&f.root, "work", &f.live(), &cfg, false).unwrap();
        assert_eq!(report.from.as_deref(), Some("personal"));
        let settings = fs::read_to_string(f.claude.join("settings.json")).unwrap();
        assert!(settings.contains("midnight"));
        assert!(f.claude.join("skills/review/SKILL.md").exists());
        assert!(!f.claude.join("skills/cook/SKILL.md").exists());
        let cj: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&f.claude_json).unwrap()).unwrap();
        assert_eq!(cj["mcpServers"]["fetch"]["command"], "uvx");
    }

    #[test]
    fn switch_refuses_new_hooks_non_interactively() {
        let f = Fixture::new();
        let cfg = Config::default();
        create(&f.root, "work", None, &cfg, Some(&f.live())).unwrap();
        switch(&f.root, "work", &f.live(), &cfg, true).unwrap();

        create(&f.root, "sneaky", None, &cfg, None).unwrap();
        write(
            &data_dir(&f.root, "sneaky").join("settings.json"),
            r#"{"hooks":{"SessionStart":[{"hooks":[{"type":"command","command":"evil"}]}]}}"#,
        );
        let err = switch(&f.root, "sneaky", &f.live(), &cfg, true).unwrap_err();
        assert!(err.to_string().contains("hooks"), "got: {err:#}");
        // Still on work; live settings untouched.
        assert_eq!(active(&f.root).unwrap().unwrap().name, "work");
        assert!(!fs::read_to_string(f.claude.join("settings.json"))
            .unwrap()
            .contains("evil"));
    }

    #[test]
    fn failed_switch_rolls_back_from_backup() {
        let f = Fixture::new();
        let cfg = Config::default();
        create(&f.root, "work", None, &cfg, Some(&f.live())).unwrap();
        switch(&f.root, "work", &f.live(), &cfg, false).unwrap();

        create(&f.root, "broken", None, &cfg, None).unwrap();
        // A credential file in the store makes apply_profile fail mid-way,
        // after settings.json was already swapped.
        write(
            &data_dir(&f.root, "broken").join("settings.json"),
            r#"{"theme":"broken"}"#,
        );
        write(
            &data_dir(&f.root, "broken").join("skills/.credentials.json"),
            "{}",
        );

        let err = switch(&f.root, "broken", &f.live(), &cfg, false).unwrap_err();
        assert!(err.to_string().contains("restored"), "got: {err:#}");
        // Live state rolled back to work's.
        let settings = fs::read_to_string(f.claude.join("settings.json")).unwrap();
        assert!(settings.contains("dark"), "got: {settings}");
        assert!(f.claude.join("skills/review/SKILL.md").exists());
        // Journal still points at work.
        assert_eq!(active(&f.root).unwrap().unwrap().name, "work");
    }

    #[test]
    fn rollback_reverts_to_previous_profile() {
        let f = Fixture::new();
        let cfg = Config::default();
        create(&f.root, "work", None, &cfg, Some(&f.live())).unwrap();
        switch(&f.root, "work", &f.live(), &cfg, false).unwrap();
        create(&f.root, "personal", None, &cfg, None).unwrap();
        write(
            &data_dir(&f.root, "personal").join("settings.json"),
            r#"{"theme":"light"}"#,
        );
        switch(&f.root, "personal", &f.live(), &cfg, false).unwrap();

        let msg = rollback(&f.root, &f.live(), &cfg, false).unwrap();
        assert!(msg.contains("work"), "got: {msg}");
        assert_eq!(active(&f.root).unwrap().unwrap().name, "work");
        assert!(fs::read_to_string(f.claude.join("settings.json"))
            .unwrap()
            .contains("dark"));
    }

    #[test]
    fn delete_refuses_active_profile() {
        let f = Fixture::new();
        let cfg = Config::default();
        create(&f.root, "work", None, &cfg, None).unwrap();
        switch(&f.root, "work", &f.live(), &cfg, false).unwrap();
        assert!(delete(&f.root, "work").is_err());
        create(&f.root, "other", None, &cfg, None).unwrap();
        delete(&f.root, "other").unwrap();
        assert_eq!(list(&f.root).unwrap(), vec!["work"]);
    }

    #[test]
    fn diff_live_reports_component_level_changes() {
        let f = Fixture::new();
        let cfg = Config::default();
        create(&f.root, "work", None, &cfg, Some(&f.live())).unwrap();
        assert!(diff_live(&f.root, "work", &f.live(), &cfg)
            .unwrap()
            .is_empty());

        write(&f.claude.join("settings.json"), r#"{"theme":"new"}"#);
        write(&f.claude.join("skills/extra/SKILL.md"), "# extra");
        let entries = diff_live(&f.root, "work", &f.live(), &cfg).unwrap();
        let find = |rel: &str| entries.iter().find(|e| e.rel == rel).unwrap();
        assert_eq!(find("settings.json").state, DiffState::Changed);
        assert_eq!(find("skills/extra/SKILL.md").state, DiffState::LocalOnly);
    }

    #[test]
    fn rejects_escaping_or_shared_components() {
        let f = Fixture::new();
        let cfg = Config::default();
        create(&f.root, "work", None, &cfg, Some(&f.live())).unwrap();
        switch(&f.root, "work", &f.live(), &cfg, false).unwrap();
        // A victim file outside ~/.claude that an escaping entry would hit.
        let victim = f.claude.parent().unwrap().join(".bashrc");
        write(&victim, "keep me");

        for bad in [
            "../.bashrc",
            "/etc/passwd",
            "projects",
            "Projects",
            "todos",
            PROFILES_COMPONENT,
            ".credentials.json",
            "skills/../../x",
            "a/b",
            ".",
            "..",
            "",
        ] {
            assert!(validate_component(bad).is_err(), "accepted {bad:?}");
        }
        for good in ["settings.json", "skills", "CLAUDE.md", "output-styles"] {
            validate_component(good).unwrap();
        }

        // A synced profile.toml with an escaping entry fails loudly on load
        // and on switch, and nothing is touched.
        create(&f.root, "evil", None, &cfg, None).unwrap();
        fs::write(
            meta_path(&f.root, "evil"),
            "components = [\"settings.json\", \"../.bashrc\", \"projects\"]\n",
        )
        .unwrap();
        assert!(read_meta(&f.root, "evil").is_err());
        assert!(components(&f.root, "evil", &cfg).is_err());
        assert!(switch(&f.root, "evil", &f.live(), &cfg, false).is_err());
        assert_eq!(fs::read_to_string(&victim).unwrap(), "keep me");
        assert!(f.claude.join("projects/-home-x-p/s.jsonl").exists());
        assert_eq!(active(&f.root).unwrap().unwrap().name, "work");

        // A bad config default is refused too.
        let mut bad_cfg = Config::default();
        bad_cfg.profiles.components = vec!["todos".into()];
        assert!(components(&f.root, "work", &bad_cfg).is_err());
    }

    #[test]
    fn rollback_gates_hooks_of_replaced_previous_store() {
        let f = Fixture::new();
        let cfg = Config::default();
        create(&f.root, "work", None, &cfg, Some(&f.live())).unwrap();
        switch(&f.root, "work", &f.live(), &cfg, true).unwrap();
        create(&f.root, "personal", None, &cfg, None).unwrap();
        switch(&f.root, "personal", &f.live(), &cfg, true).unwrap();

        // A later pull replaced work's store with one that installs a hook.
        write(
            &data_dir(&f.root, "work").join("settings.json"),
            r#"{"hooks":{"SessionStart":[{"hooks":[{"type":"command","command":"evil"}]}]}}"#,
        );
        let err = rollback(&f.root, &f.live(), &cfg, true).unwrap_err();
        assert!(err.to_string().contains("hooks"), "got: {err:#}");
        assert_eq!(active(&f.root).unwrap().unwrap().name, "personal");
        assert!(!fs::read_to_string(f.claude.join("settings.json"))
            .map(|s| s.contains("evil"))
            .unwrap_or(false));
    }

    #[test]
    fn rollback_without_previous_captures_back_and_keeps_claude_json() {
        let f = Fixture::new();
        let cfg = Config::default();
        create(&f.root, "work", None, &cfg, None).unwrap();
        write(
            &data_dir(&f.root, "work").join("settings.json"),
            r#"{"theme":"work"}"#,
        );
        write(
            &mcp_path(&f.root, "work"),
            r#"{"mcpServers":{"jira":{"command":"jira-mcp"}}}"#,
        );
        // First switch: no previous profile, pre-switch state is backed up.
        let report = switch(&f.root, "work", &f.live(), &cfg, false).unwrap();
        assert!(report.backup_dir.is_some());

        // Edit live while `work` is active; Claude Code refreshes OAuth.
        write(&f.claude.join("settings.json"), r#"{"theme":"edited"}"#);
        let mut cj: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&f.claude_json).unwrap()).unwrap();
        cj["oauthAccount"]["t"] = "refreshed".into();
        fs::write(&f.claude_json, serde_json::to_string(&cj).unwrap()).unwrap();

        let msg = rollback(&f.root, &f.live(), &cfg, false).unwrap();
        assert!(msg.contains("restored"), "got: {msg}");
        assert!(active(&f.root).unwrap().is_none());

        // Pre-switch components are back.
        let settings = fs::read_to_string(f.claude.join("settings.json")).unwrap();
        assert!(settings.contains("dark"), "got: {settings}");
        // The edit was captured back into work's store, not lost.
        let stored = fs::read_to_string(data_dir(&f.root, "work").join("settings.json")).unwrap();
        assert!(stored.contains("edited"), "got: {stored}");
        // Only user-scope MCP servers were restored; refreshed OAuth survives.
        let cj: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&f.claude_json).unwrap()).unwrap();
        assert_eq!(cj["oauthAccount"]["t"], "refreshed");
        assert_eq!(cj["mcpServers"]["fetch"]["command"], "uvx");
        assert!(cj["mcpServers"].get("jira").is_none());
    }

    #[test]
    fn failed_switch_restores_only_mcp_servers_from_backup() {
        let f = Fixture::new();
        let cfg = Config::default();
        create(&f.root, "work", None, &cfg, Some(&f.live())).unwrap();
        switch(&f.root, "work", &f.live(), &cfg, false).unwrap();
        create(&f.root, "broken", None, &cfg, None).unwrap();
        write(
            &data_dir(&f.root, "broken").join("skills/.credentials.json"),
            "{}",
        );
        let backup = live_backup_state(&f);
        // Simulate a concurrent token refresh right after the backup would be
        // taken: restore_backup must not revert unrelated keys.
        let mut cj: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&f.claude_json).unwrap()).unwrap();
        cj["oauthAccount"]["t"] = "refreshed".into();
        cj["mcpServers"] = serde_json::json!({"other": {"command": "x"}});
        fs::write(&f.claude_json, serde_json::to_string(&cj).unwrap()).unwrap();
        restore_backup(&f.live(), Some(&backup), &["settings.json".into()], &cfg).unwrap();
        let cj: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&f.claude_json).unwrap()).unwrap();
        assert_eq!(cj["oauthAccount"]["t"], "refreshed");
        assert_eq!(cj["mcpServers"]["fetch"]["command"], "uvx");
        assert!(cj["mcpServers"].get("other").is_none());

        // And the real failed-switch path still rolls back cleanly.
        assert!(switch(&f.root, "broken", &f.live(), &cfg, false).is_err());
        let cj: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&f.claude_json).unwrap()).unwrap();
        assert_eq!(cj["oauthAccount"]["t"], "refreshed");
        assert_eq!(cj["mcpServers"]["fetch"]["command"], "uvx");
    }

    fn live_backup_state(f: &Fixture) -> PathBuf {
        backup_components(&f.live(), &["settings.json".into()], &Config::default())
            .unwrap()
            .unwrap()
    }

    #[test]
    fn backups_in_same_second_get_distinct_dirs() {
        let f = Fixture::new();
        let cfg = Config::default();
        let comps = vec!["settings.json".to_string()];
        let a = backup_components(&f.live(), &comps, &cfg).unwrap().unwrap();
        write(&f.claude.join("settings.json"), r#"{"theme":"second"}"#);
        let b = backup_components(&f.live(), &comps, &cfg).unwrap().unwrap();
        let c = backup_components(&f.live(), &comps, &cfg).unwrap().unwrap();
        assert_ne!(a, b);
        assert_ne!(b, c);
        assert_ne!(a, c);
        // The first backup is not layered over by the later ones.
        assert!(fs::read_to_string(a.join("settings.json"))
            .unwrap()
            .contains("dark"));
        assert!(fs::read_to_string(b.join("settings.json"))
            .unwrap()
            .contains("second"));

        // Nothing to back up -> no stray empty dir left behind.
        let empty = LiveState {
            claude_dir: &f.claude,
            claude_json: None,
        };
        assert!(backup_components(&empty, &["nope".into()], &cfg)
            .unwrap()
            .is_none());
    }

    #[cfg(unix)]
    #[test]
    fn symlinks_in_owned_components_survive_switch_round_trip() {
        use std::os::unix::fs::symlink;
        let f = Fixture::new();
        let cfg = Config::default();
        let dotfiles = f.claude.parent().unwrap().join("dotfiles/foo");
        write(&dotfiles.join("SKILL.md"), "# foo from dotfiles");
        symlink(&dotfiles, f.claude.join("skills/foo")).unwrap();
        // A dangling link is preserved too.
        symlink(
            "/nonexistent/ccsync-target",
            f.claude.join("skills/dangling"),
        )
        .unwrap();

        create(&f.root, "work", None, &cfg, Some(&f.live())).unwrap();
        let stored = data_dir(&f.root, "work").join("skills/foo");
        assert!(fs::symlink_metadata(&stored)
            .unwrap()
            .file_type()
            .is_symlink());
        switch(&f.root, "work", &f.live(), &cfg, false).unwrap();

        create(&f.root, "personal", None, &cfg, None).unwrap();
        let report = switch(&f.root, "personal", &f.live(), &cfg, false).unwrap();
        // Switching away removed the link, never the dotfiles target.
        assert!(!entry_exists(&f.claude.join("skills/foo")));
        assert_eq!(
            fs::read_to_string(dotfiles.join("SKILL.md")).unwrap(),
            "# foo from dotfiles"
        );
        // The backup preserved it as a link as well.
        let backup = report.backup_dir.unwrap();
        assert!(fs::symlink_metadata(backup.join("skills/foo"))
            .unwrap()
            .file_type()
            .is_symlink());

        switch(&f.root, "work", &f.live(), &cfg, false).unwrap();
        let live_link = f.claude.join("skills/foo");
        assert!(fs::symlink_metadata(&live_link)
            .unwrap()
            .file_type()
            .is_symlink());
        assert_eq!(fs::read_link(&live_link).unwrap(), dotfiles);
        assert_eq!(
            fs::read_link(f.claude.join("skills/dangling")).unwrap(),
            PathBuf::from("/nonexistent/ccsync-target")
        );
    }

    #[cfg(unix)]
    #[test]
    fn apply_never_writes_through_live_symlink() {
        use std::os::unix::fs::symlink;
        let f = Fixture::new();
        let cfg = Config::default();
        create(&f.root, "work", None, &cfg, None).unwrap();
        write(
            &data_dir(&f.root, "work").join("settings.json"),
            r#"{"theme":"work"}"#,
        );
        // Live settings.json is a symlink to a file outside ~/.claude.
        let outside = f.claude.parent().unwrap().join("outside.json");
        write(&outside, "original");
        fs::remove_file(f.claude.join("settings.json")).unwrap();
        symlink(&outside, f.claude.join("settings.json")).unwrap();

        switch(&f.root, "work", &f.live(), &cfg, false).unwrap();
        assert_eq!(fs::read_to_string(&outside).unwrap(), "original");
        let live = f.claude.join("settings.json");
        assert!(!fs::symlink_metadata(&live)
            .unwrap()
            .file_type()
            .is_symlink());
        assert!(fs::read_to_string(&live).unwrap().contains("work"));
    }
}
