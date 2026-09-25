//! Git transport. The snapshot (manifest + `data/`) is mirrored into a local
//! clone of a remote repository and pushed; pulling does the inverse. We shell
//! out to the system `git` binary rather than linking libgit2 — it keeps the
//! build dependency-free and matches whatever auth (ssh keys, credential
//! helpers) the user already has configured.
//!
//! Repo layout: each machine owns a `machines/<machine-id>/` subtree holding
//! its `manifest.json` + `data/`, so several machines share one remote without
//! clobbering each other; the git history doubles as per-snapshot versioning.
//! Repos written before this layout kept a single snapshot at the root; the
//! first push from a current version moves that legacy root snapshot to
//! `machines/default/`.
//!
//! The local clone is cached under `~/.config/ccsync/repo` so subsequent
//! pushes/pulls are incremental.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result};
use walkdir::WalkDir;

use crate::error::CcError;
use crate::manifest::{Manifest, MANIFEST_NAME};

/// Directory under the repo root holding one subtree per machine.
pub const MACHINES_DIR: &str = "machines";
/// Machine id assigned to a legacy root-level snapshot on migration.
pub const LEGACY_MACHINE: &str = "default";

/// Local cache clone location.
pub fn repo_cache() -> Result<PathBuf> {
    let base = dirs::config_dir().ok_or(CcError::ClaudeDirNotFound)?;
    Ok(base.join("ccsync").join("repo"))
}

/// The repo cache plus an exclusive lock on it, held until the guard drops,
/// so a daemon tick and an interactive push/pull never interleave
/// `reset --hard`/`add`/`commit` on the same checkout. Each public entry
/// point takes it exactly once; none of them calls another.
fn locked_cache() -> Result<(PathBuf, std::fs::File)> {
    let cache = repo_cache()?;
    let lock = crate::lock::exclusive(&cache.with_extension("lock"))?;
    Ok((cache, lock))
}

fn run_git(args: &[&str], cwd: Option<&Path>) -> Result<String> {
    let mut cmd = Command::new("git");
    // The remote URL is user- or config-supplied: restrict git to real
    // transports so exotic schemes like `ext::sh -c ...` can never execute
    // commands, whatever the URL says.
    cmd.env("GIT_ALLOW_PROTOCOL", "ssh:https:http:file");
    cmd.args(["-c", "protocol.ext.allow=never"]);
    cmd.args(args);
    if let Some(dir) = cwd {
        cmd.current_dir(dir);
    }
    let out = cmd
        .output()
        .with_context(|| format!("running `git {}`", args.join(" ")))?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        return Err(CcError::Git(format!("git {}: {}", args.join(" "), stderr.trim())).into());
    }
    Ok(String::from_utf8_lossy(&out.stdout).to_string())
}

/// How a cache refresh treats fetch/reset failures.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Sync {
    /// Read paths (pull, history, machines, diff --remote, layers): a failed
    /// fetch must surface, never serve the stale cache as if it were current.
    Strict,
    /// Push: offline or brand-new remotes still stage a local commit, and the
    /// push itself (plus its one re-align retry) reports connectivity errors.
    BestEffort,
}

/// Attribute rules that pin every path to raw bytes: no eol conversion, no
/// clean/smudge filters, no `$Id$` expansion, no re-encoding.
const RAW_ATTRIBUTES: &str = "* -text -filter -ident -working-tree-encoding\n";

/// Committed at the repo root so other clones (and other tools) also treat
/// every file as opaque bytes.
const ROOT_GITATTRIBUTES: &str = "* -text\n";

/// The URL `origin` is configured with, verbatim (unlike `remote get-url`,
/// which applies `insteadOf` rewrites).
fn configured_origin(cache: &Path) -> Option<String> {
    run_git(&["config", "--get", "remote.origin.url"], Some(cache))
        .ok()
        .map(|s| s.trim().to_string())
}

/// Whether two remote URLs name the same repo. `git clone ./r.git` records an
/// absolute path, so local paths compare by their canonical form; without
/// that a relative remote would re-clone the cache on every command.
fn same_remote(a: &str, b: &str) -> bool {
    if a == b {
        return true;
    }
    match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

/// Ensure the local cache is a clone of `remote`, aligned with the remote tip
/// if it already exists.
fn ensure_clone(remote: &str, cache: &Path, mode: Sync) -> Result<()> {
    if cache.join(".git").exists()
        && !configured_origin(cache).is_some_and(|origin| same_remote(&origin, remote))
    {
        // The cache was cloned from a different remote (a --remote override
        // or a changed config). Its `origin/*` refs and local history belong
        // to the old remote: re-pointing the URL and fetching would leave
        // those stale refs in place, so an empty new remote would get the old
        // remote's history pushed into it. The cache is disposable — re-clone.
        std::fs::remove_dir_all(cache)
            .with_context(|| format!("removing stale repo cache {}", cache.display()))?;
    }
    if cache.join(".git").exists() {
        pin_raw_attributes(cache)?;
        align_with_remote(cache, mode)?;
    } else {
        if let Some(parent) = cache.parent() {
            std::fs::create_dir_all(parent)?;
        }
        // Check out only after pinning attributes, so even the first
        // checkout writes bytes exactly as committed.
        run_git(
            &[
                "clone",
                "--no-checkout",
                "--",
                remote,
                &cache.to_string_lossy(),
            ],
            None,
        )?;
        pin_raw_attributes(cache)?;
        if run_git(&["rev-parse", "--verify", "--quiet", "HEAD"], Some(cache)).is_ok() {
            run_git(&["reset", "--hard", "HEAD"], Some(cache))?;
        }
    }
    Ok(())
}

/// Write the raw-bytes attribute rules into `.git/info/attributes`, which
/// outranks every in-tree `.gitattributes` (including ones inside captured
/// skill dirs) and the user's global `core.attributesFile`, so neither
/// checkout nor add can alter file bytes.
fn pin_raw_attributes(cache: &Path) -> Result<()> {
    let info = cache.join(".git").join("info");
    std::fs::create_dir_all(&info)?;
    std::fs::write(info.join("attributes"), RAW_ATTRIBUTES)?;
    Ok(())
}

/// Move the cache's branch to the remote tip. Snapshots replace the whole
/// repo state on every push (last writer wins), so the cache's own history is
/// disposable — `reset --hard` instead of merge/ff means a cache that diverged
/// from the remote (two machines pushing) can always recover. A remote with no
/// commits yet leaves the cache as is. In [`Sync::BestEffort`] mode fetch and
/// reset failures are ignored so offline pushes can still stage a commit; in
/// [`Sync::Strict`] mode they propagate.
fn align_with_remote(cache: &Path, mode: Sync) -> Result<()> {
    let strict = mode == Sync::Strict;
    // --prune drops remote-tracking refs for branches the remote no longer has.
    if let Err(e) = run_git(&["fetch", "--prune", "origin"], Some(cache)) {
        if strict {
            return Err(e.context("refreshing the repo cache from the remote"));
        }
        return Ok(());
    }
    if let Ok(branch) = run_git(&["symbolic-ref", "--short", "HEAD"], Some(cache)) {
        let remote_ref = format!("origin/{}", branch.trim());
        if run_git(&["rev-parse", "--verify", &remote_ref], Some(cache)).is_ok() {
            let reset = run_git(&["reset", "--hard", &remote_ref], Some(cache));
            if strict {
                reset?;
            }
        }
    }
    Ok(())
}

/// Move a legacy root-level snapshot (pre-machines layout) into
/// `machines/default/` so it stays visible as a machine instead of clobbering
/// or being clobbered by subtree pushes.
fn migrate_legacy_root(cache: &Path) -> Result<()> {
    if !cache.join(MANIFEST_NAME).exists() {
        return Ok(());
    }
    let legacy = cache.join(MACHINES_DIR).join(LEGACY_MACHINE);
    if legacy.exists() {
        // Already have a `default` machine; the root copy is stale — drop it.
        let _ = std::fs::remove_file(cache.join(MANIFEST_NAME));
        let _ = std::fs::remove_dir_all(cache.join("data"));
        return Ok(());
    }
    std::fs::create_dir_all(&legacy)?;
    std::fs::rename(cache.join(MANIFEST_NAME), legacy.join(MANIFEST_NAME))?;
    if cache.join("data").exists() {
        std::fs::rename(cache.join("data"), legacy.join("data"))?;
    }
    Ok(())
}

/// Replace this machine's subtree with the staged snapshot (so deletions
/// propagate) and commit if anything changed. A snapshot identical to what the
/// cache already holds commits nothing; the follow-up push is then a cheap
/// no-op against an up-to-date remote, or publishes the existing history to a
/// remote that does not have it yet.
fn overlay_and_commit(staging: &Path, cache: &Path, machine_id: &str) -> Result<()> {
    let staged_data = staging.join("data");
    reject_nested_git(&staged_data)?;
    let manifest = Manifest::read_from(staging)?;

    migrate_legacy_root(cache)?;

    let subtree = cache.join(MACHINES_DIR).join(machine_id);
    let _ = std::fs::remove_dir_all(&subtree);
    copy_tree(&staging.join(MANIFEST_NAME), &subtree.join(MANIFEST_NAME))?;
    if staged_data.exists() {
        copy_tree(&staged_data, &subtree.join("data"))?;
    }

    let attrs = cache.join(".gitattributes");
    if std::fs::read_to_string(&attrs).ok().as_deref() != Some(ROOT_GITATTRIBUTES) {
        std::fs::write(&attrs, ROOT_GITATTRIBUTES)?;
    }

    // Captured trees (skills especially) can carry their own `.gitignore`
    // files, and the user's global excludes file applies to every repo:
    // either would silently drop snapshot files. Point core.excludesFile at
    // an empty file (portable, unlike /dev/null vs NUL) and --force the add
    // so no ignore rule of any origin can exclude a staged file.
    let empty_excludes = cache.join(".git").join("ccsync-empty-excludes");
    std::fs::write(&empty_excludes, "")?;
    let excludes_opt = format!("core.excludesFile={}", empty_excludes.display());
    run_git(&["-c", &excludes_opt, "add", "-A", "--force"], Some(cache))?;

    let status = run_git(&["status", "--porcelain"], Some(cache))?;
    if status.trim().is_empty() {
        return verify_tracked(cache, machine_id, &manifest);
    }
    let msg = format!(
        "ccsync snapshot [{machine_id}] {}",
        chrono::Utc::now().to_rfc3339()
    );
    // Provide a committer identity inline so backups work even on machines
    // where git's global user.name/user.email are not configured.
    run_git(
        &[
            "-c",
            "user.name=ccsync",
            "-c",
            "user.email=ccsync@localhost",
            "commit",
            "--no-verify",
            "-m",
            &msg,
        ],
        Some(cache),
    )?;
    verify_tracked(cache, machine_id, &manifest)
}

/// Refuse staged data containing a `.git` path component. Git cannot store
/// such paths: a nested repository becomes an empty gitlink (or is refused),
/// so its files would vanish from the remote while the manifest still lists
/// them, and every later restore would fail integrity verification. Silently
/// skipping them here would produce the same broken snapshot, so fail loudly
/// and name the path the user needs to exclude.
fn reject_nested_git(staged_data: &Path) -> Result<()> {
    if !staged_data.exists() {
        return Ok(());
    }
    for entry in WalkDir::new(staged_data).follow_links(false) {
        let entry = entry?;
        let rel = entry
            .path()
            .strip_prefix(staged_data)
            .unwrap_or(entry.path());
        if entry
            .file_name()
            .to_str()
            .is_some_and(|n| n.eq_ignore_ascii_case(".git"))
        {
            let rel = rel.to_string_lossy().replace('\\', "/");
            return Err(CcError::Git(format!(
                "snapshot contains a nested git repository at `{rel}`; git cannot store \
                 `.git` paths, so it would be silently dropped from the backup. Add `{rel}` \
                 to `exclude` in the ccsync config (or use --archive) and re-run `ccsync snapshot`"
            ))
            .into());
        }
    }
    Ok(())
}

/// Confirm every file the manifest promises is actually tracked in this
/// machine's subtree, so nothing (ignore rules, gitlinks, git refusing a
/// path) can silently thin the backup.
fn verify_tracked(cache: &Path, machine_id: &str, manifest: &Manifest) -> Result<()> {
    let prefix = format!("{MACHINES_DIR}/{machine_id}/data/");
    let listed = run_git(&["ls-files", "-z", "--", &prefix], Some(cache))?;
    let tracked: std::collections::HashSet<&str> = listed
        .split('\0')
        .filter_map(|p| p.strip_prefix(prefix.as_str()))
        .collect();
    let missing: Vec<&str> = manifest
        .files
        .iter()
        .map(|f| f.rel_path.as_str())
        .filter(|p| !tracked.contains(p))
        .collect();
    if !missing.is_empty() {
        let shown: Vec<&str> = missing.iter().take(10).copied().collect();
        return Err(CcError::Git(format!(
            "{} manifest file(s) were not committed to the repo (e.g. {}); \
             the pushed snapshot would be incomplete",
            missing.len(),
            shown.join(", ")
        ))
        .into());
    }
    Ok(())
}

/// Push the staged snapshot to this machine's subtree on the git `remote`.
pub fn push(remote: &str, staging: &Path, machine_id: &str) -> Result<()> {
    let (cache, _lock) = locked_cache()?;
    push_with_cache(remote, staging, &cache, machine_id)
}

pub(crate) fn push_with_cache(
    remote: &str,
    staging: &Path,
    cache: &Path,
    machine_id: &str,
) -> Result<()> {
    ensure_clone(remote, cache, Sync::BestEffort)?;
    overlay_and_commit(staging, cache, machine_id)?;
    if run_git(&["push", "-u", "origin", "HEAD"], Some(cache)).is_ok() {
        return Ok(());
    }
    // Rejected — another machine pushed between our fetch and push. Re-align
    // to the new remote tip, re-overlay the snapshot, and retry exactly once;
    // a second rejection is surfaced to the caller. Each machine writes only
    // its own subtree, so the re-overlay cannot lose the other machine's push.
    align_with_remote(cache, Sync::BestEffort)?;
    overlay_and_commit(staging, cache, machine_id)?;
    run_git(&["push", "-u", "origin", "HEAD"], Some(cache))?;
    Ok(())
}

/// The snapshot subtrees present in the cache, as `(machine_id, dir)` pairs.
/// A legacy root-level snapshot appears as [`LEGACY_MACHINE`].
fn subtrees_in(cache: &Path) -> Vec<(String, PathBuf)> {
    let mut out = Vec::new();
    let machines = cache.join(MACHINES_DIR);
    if let Ok(entries) = std::fs::read_dir(&machines) {
        for entry in entries.flatten() {
            let dir = entry.path();
            if dir.join(MANIFEST_NAME).exists() {
                if let Some(name) = entry.file_name().to_str() {
                    out.push((name.to_string(), dir));
                }
            }
        }
    }
    if cache.join(MANIFEST_NAME).exists() && !out.iter().any(|(n, _)| n == LEGACY_MACHINE) {
        out.push((LEGACY_MACHINE.to_string(), cache.to_path_buf()));
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

/// Pick the subtree a pull should read: an explicit `--from` machine wins,
/// else this machine's own subtree, else the only machine present. Ambiguity
/// or absence errors with the available machine names.
fn select_subtree(cache: &Path, from: Option<&str>, own_id: &str) -> Result<PathBuf> {
    let subtrees = subtrees_in(cache);
    let names = || {
        subtrees
            .iter()
            .map(|(n, _)| n.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    };
    if let Some(from) = from {
        return subtrees
            .iter()
            .find(|(n, _)| n == from)
            .map(|(_, d)| d.clone())
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "no snapshot from machine {from:?} (have: {}); see `ccsync machines`",
                    names()
                )
            });
    }
    if let Some((_, dir)) = subtrees.iter().find(|(n, _)| n == own_id) {
        return Ok(dir.clone());
    }
    match subtrees.len() {
        0 => Err(CcError::NoStagedSnapshot("the remote".to_string()).into()),
        1 => Ok(subtrees[0].1.clone()),
        _ => Err(anyhow::anyhow!(
            "multiple machines have snapshots ({}); pick one with `ccsync pull --from <machine>`",
            names()
        )),
    }
}

/// Pull the latest snapshot from `remote` into the `staging` directory.
/// `from` selects another machine's subtree; default is this machine's own
/// (or the only one present).
pub fn pull(remote: &str, staging: &Path, from: Option<&str>, own_id: &str) -> Result<()> {
    let (cache, _lock) = locked_cache()?;
    pull_with_cache(remote, staging, &cache, from, own_id)
}

pub(crate) fn pull_with_cache(
    remote: &str,
    staging: &Path,
    cache: &Path,
    from: Option<&str>,
    own_id: &str,
) -> Result<()> {
    ensure_clone(remote, cache, Sync::Strict)?;
    let subtree = select_subtree(cache, from, own_id)?;
    copy_snapshot(&subtree, staging)
}

/// Pull the snapshot as of `commit` (from `ccsync history`) into `staging`,
/// leaving the cache back at the remote tip afterwards.
pub fn pull_at(
    remote: &str,
    commit: &str,
    staging: &Path,
    from: Option<&str>,
    own_id: &str,
) -> Result<()> {
    let (cache, _lock) = locked_cache()?;
    pull_at_with_cache(remote, commit, staging, &cache, from, own_id)
}

pub(crate) fn pull_at_with_cache(
    remote: &str,
    commit: &str,
    staging: &Path,
    cache: &Path,
    from: Option<&str>,
    own_id: &str,
) -> Result<()> {
    // The commit is a positional arg to `reset`; only accept hash-shaped
    // input so it can never be mistaken for an option or a ref expression.
    if commit.is_empty() || !commit.chars().all(|c| c.is_ascii_hexdigit()) {
        anyhow::bail!("invalid commit {commit:?}: pass a hash from `ccsync history`");
    }
    ensure_clone(remote, cache, Sync::Strict)?;
    run_git(&["rev-parse", "--verify", "--quiet", commit], Some(cache))
        .map_err(|_| anyhow::anyhow!("commit {commit} not found; see `ccsync history`"))?;
    run_git(&["reset", "--hard", commit], Some(cache))?;
    let result =
        select_subtree(cache, from, own_id).and_then(|subtree| copy_snapshot(&subtree, staging));
    // Whatever happened, put the cache back on the remote tip. Best-effort:
    // the snapshot is already copied, and the next strict read re-aligns.
    let _ = align_with_remote(cache, Sync::BestEffort);
    result
}

/// Materialize the snapshot in `subtree` into `staging`. It is copied beside
/// staging first and swapped in whole, so a failed copy never leaves staging
/// wiped or half-filled.
fn copy_snapshot(subtree: &Path, staging: &Path) -> Result<()> {
    crate::snapshot::replace_staging(staging, |fresh| {
        copy_tree(&subtree.join(MANIFEST_NAME), &fresh.join(MANIFEST_NAME))?;
        let repo_data = subtree.join("data");
        if repo_data.exists() {
            copy_tree(&repo_data, &fresh.join("data"))?;
        }
        Ok(())
    })
}

/// Refresh the local cache from `remote` (cloning it if needed) without
/// copying anything into staging. Used before reading history/manifests.
pub fn refresh_cache(remote: &str) -> Result<()> {
    let (cache, _lock) = locked_cache()?;
    ensure_clone(remote, &cache, Sync::Strict)
}

/// Clone `remote` into `dest` or fast-forward an existing checkout to the
/// remote tip. Used for read-only layer checkouts, which are plain repos
/// rather than snapshot stores.
pub fn clone_or_update(remote: &str, dest: &Path) -> Result<()> {
    ensure_clone(remote, dest, Sync::Strict)
}

/// Read a machine's manifest from the remote without transferring snapshot
/// data into staging. Backs `ccsync diff --remote`.
pub fn remote_manifest(remote: &str, from: Option<&str>, own_id: &str) -> Result<Manifest> {
    let (cache, _lock) = locked_cache()?;
    remote_manifest_with_cache(remote, &cache, from, own_id)
}

pub(crate) fn remote_manifest_with_cache(
    remote: &str,
    cache: &Path,
    from: Option<&str>,
    own_id: &str,
) -> Result<Manifest> {
    ensure_clone(remote, cache, Sync::Strict)?;
    let subtree = select_subtree(cache, from, own_id)?;
    Manifest::read_from(&subtree)
}

/// Every machine with a snapshot on the remote, with its manifest (source
/// host, timestamp, file count, producing version).
pub fn machines(remote: &str) -> Result<Vec<(String, Manifest)>> {
    let (cache, _lock) = locked_cache()?;
    machines_with_cache(remote, &cache)
}

pub(crate) fn machines_with_cache(remote: &str, cache: &Path) -> Result<Vec<(String, Manifest)>> {
    ensure_clone(remote, cache, Sync::Strict)?;
    let mut out = Vec::new();
    for (name, dir) in subtrees_in(cache) {
        if let Ok(manifest) = Manifest::read_from(&dir) {
            out.push((name, manifest));
        }
    }
    Ok(out)
}

/// Copy a file or directory tree from `src` to `dst`.
fn copy_tree(src: &Path, dst: &Path) -> Result<()> {
    if src.is_file() {
        if let Some(parent) = dst.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::copy(src, dst)?;
        return Ok(());
    }
    for entry in WalkDir::new(src).follow_links(false) {
        let entry = entry?;
        if entry.file_type().is_file() {
            let rel = entry.path().strip_prefix(src).unwrap();
            // The clone contents come from a remote; never let a crafted path
            // write outside `dst`.
            if !rel
                .components()
                .all(|c| matches!(c, std::path::Component::Normal(_)))
            {
                return Err(CcError::Git(format!("unsafe path in repo: {}", rel.display())).into());
            }
            let target = dst.join(rel);
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::copy(entry.path(), &target)?;
        }
    }
    Ok(())
}

/// Recent snapshot commits in the local repo cache, newest first. Each tuple is
/// `(short_hash, committer_date_iso8601, subject)`. Returns an empty list when
/// the cache has no commits yet; errors only if `git log` itself fails.
pub fn log(limit: usize) -> Result<Vec<(String, String, String)>> {
    let (cache, _lock) = locked_cache()?;
    log_with_cache(&cache, limit)
}

pub(crate) fn log_with_cache(cache: &Path, limit: usize) -> Result<Vec<(String, String, String)>> {
    if !cache.join(".git").exists() {
        return Ok(Vec::new());
    }
    let n = limit.to_string();
    // A repo with no commits makes `git log` exit non-zero; treat that as empty.
    let out = match run_git(&["log", "--pretty=%h|%cI|%s", "-n", &n], Some(cache)) {
        Ok(out) => out,
        Err(_) => return Ok(Vec::new()),
    };
    let mut commits = Vec::new();
    for line in out.lines() {
        let mut parts = line.splitn(3, '|');
        let hash = parts.next().unwrap_or("").to_string();
        let date = parts.next().unwrap_or("").to_string();
        let subject = parts.next().unwrap_or("").to_string();
        if !hash.is_empty() {
            commits.push((hash, date, subject));
        }
    }
    Ok(commits)
}

/// Resolve the effective remote: explicit arg wins, else config.
pub fn resolve_remote(explicit: Option<&str>, config_remote: Option<&str>) -> Result<String> {
    explicit
        .or(config_remote)
        .map(|s| s.to_string())
        .ok_or_else(|| CcError::NoRemote.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A local bare repo standing in for the remote.
    fn init_bare(dir: &Path) -> String {
        std::fs::create_dir_all(dir).unwrap();
        run_git(&["init", "--bare", &dir.to_string_lossy()], None).unwrap();
        dir.to_string_lossy().to_string()
    }

    fn write_staging(staging: &Path, content: &str) {
        std::fs::create_dir_all(staging.join("data")).unwrap();
        Manifest::new("test-host".into(), "/home/test".into())
            .write_to(staging)
            .unwrap();
        std::fs::write(staging.join("data/settings.json"), content).unwrap();
    }

    #[test]
    fn push_pull_roundtrip_via_bare_remote() {
        let tmp = tempfile::tempdir().unwrap();
        let remote = init_bare(&tmp.path().join("remote.git"));

        let staging = tmp.path().join("staging");
        write_staging(&staging, r#"{"theme":"dark"}"#);
        push_with_cache(&remote, &staging, &tmp.path().join("cache-a"), "mach-a").unwrap();

        // Pull through a different cache, as a second machine would: with a
        // single machine present, its snapshot is the default.
        let pulled = tmp.path().join("pulled");
        pull_with_cache(
            &remote,
            &pulled,
            &tmp.path().join("cache-b"),
            None,
            "mach-b",
        )
        .unwrap();
        assert_eq!(
            std::fs::read_to_string(pulled.join("data/settings.json")).unwrap(),
            r#"{"theme":"dark"}"#
        );

        // Pushing the identical snapshot again is a no-op, not an error.
        push_with_cache(&remote, &staging, &tmp.path().join("cache-a"), "mach-a").unwrap();
    }

    #[test]
    fn machines_own_separate_subtrees() {
        let tmp = tempfile::tempdir().unwrap();
        let remote = init_bare(&tmp.path().join("remote.git"));
        let cache_a = tmp.path().join("cache-a");
        let cache_b = tmp.path().join("cache-b");

        let staging_a = tmp.path().join("staging-a");
        write_staging(&staging_a, "a-content");
        push_with_cache(&remote, &staging_a, &cache_a, "laptop").unwrap();
        let staging_b = tmp.path().join("staging-b");
        write_staging(&staging_b, "b-content");
        push_with_cache(&remote, &staging_b, &cache_b, "desktop").unwrap();

        // B's push must not clobber A's subtree.
        let listed = machines_with_cache(&remote, &tmp.path().join("cache-c")).unwrap();
        let names: Vec<&str> = listed.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names, vec!["desktop", "laptop"]);

        // Each machine pulls its own subtree by default; --from crosses over.
        let pulled = tmp.path().join("pulled");
        pull_with_cache(&remote, &pulled, &cache_a, None, "laptop").unwrap();
        assert_eq!(
            std::fs::read_to_string(pulled.join("data/settings.json")).unwrap(),
            "a-content"
        );
        pull_with_cache(&remote, &pulled, &cache_a, Some("desktop"), "laptop").unwrap();
        assert_eq!(
            std::fs::read_to_string(pulled.join("data/settings.json")).unwrap(),
            "b-content"
        );

        // A machine with no snapshot and no --from gets told what exists.
        let err = pull_with_cache(&remote, &pulled, &cache_a, None, "third-machine").unwrap_err();
        assert!(err.to_string().contains("desktop"), "got: {err:#}");
    }

    #[test]
    fn legacy_root_snapshot_migrates_to_default_machine() {
        let tmp = tempfile::tempdir().unwrap();
        let remote = init_bare(&tmp.path().join("remote.git"));

        // Fabricate a legacy repo: snapshot at the root, no machines/ dir.
        let seed = tmp.path().join("seed");
        run_git(&["clone", "--", &remote, &seed.to_string_lossy()], None).unwrap();
        Manifest::new("legacy-host".into(), "/home/legacy".into())
            .write_to(&seed)
            .unwrap();
        std::fs::create_dir_all(seed.join("data")).unwrap();
        std::fs::write(seed.join("data/settings.json"), "legacy-content").unwrap();
        run_git(&["add", "-A"], Some(&seed)).unwrap();
        run_git(
            &[
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "commit",
                "-m",
                "legacy",
            ],
            Some(&seed),
        )
        .unwrap();
        run_git(&["push", "origin", "HEAD"], Some(&seed)).unwrap();

        // First v2 push moves the root snapshot under machines/default and
        // adds this machine's subtree alongside it.
        let staging = tmp.path().join("staging");
        write_staging(&staging, "new-content");
        push_with_cache(&remote, &staging, &tmp.path().join("cache"), "laptop").unwrap();

        let listed = machines_with_cache(&remote, &tmp.path().join("cache-2")).unwrap();
        let names: Vec<&str> = listed.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names, vec!["default", "laptop"]);

        let pulled = tmp.path().join("pulled");
        pull_with_cache(
            &remote,
            &pulled,
            &tmp.path().join("cache-3"),
            Some("default"),
            "laptop",
        )
        .unwrap();
        assert_eq!(
            std::fs::read_to_string(pulled.join("data/settings.json")).unwrap(),
            "legacy-content"
        );
    }

    #[test]
    fn pull_at_recovers_past_snapshot_and_realigns_cache() {
        let tmp = tempfile::tempdir().unwrap();
        let remote = init_bare(&tmp.path().join("remote.git"));
        let cache = tmp.path().join("cache");

        let staging = tmp.path().join("staging");
        write_staging(&staging, "v1");
        push_with_cache(&remote, &staging, &cache, "laptop").unwrap();
        write_staging(&staging, "v2");
        push_with_cache(&remote, &staging, &cache, "laptop").unwrap();

        let commits = log_with_cache(&cache, 10).unwrap();
        assert_eq!(commits.len(), 2, "expected two snapshot commits");
        let oldest = &commits.last().unwrap().0;

        let pulled = tmp.path().join("pulled");
        pull_at_with_cache(&remote, oldest, &pulled, &cache, None, "laptop").unwrap();
        assert_eq!(
            std::fs::read_to_string(pulled.join("data/settings.json")).unwrap(),
            "v1"
        );
        // The cache is back on the tip: a plain pull yields v2 again.
        pull_with_cache(&remote, &pulled, &cache, None, "laptop").unwrap();
        assert_eq!(
            std::fs::read_to_string(pulled.join("data/settings.json")).unwrap(),
            "v2"
        );

        // Non-hash input is refused before reaching git.
        let err =
            pull_at_with_cache(&remote, "--hard", &pulled, &cache, None, "laptop").unwrap_err();
        assert!(err.to_string().contains("invalid commit"), "got: {err:#}");
    }

    #[test]
    fn remote_manifest_reads_without_touching_staging() {
        let tmp = tempfile::tempdir().unwrap();
        let remote = init_bare(&tmp.path().join("remote.git"));
        let staging = tmp.path().join("staging");
        write_staging(&staging, "content");
        push_with_cache(&remote, &staging, &tmp.path().join("cache"), "laptop").unwrap();

        let m = remote_manifest_with_cache(&remote, &tmp.path().join("cache-2"), None, "laptop")
            .unwrap();
        assert_eq!(m.source_host, "test-host");
    }

    #[test]
    fn push_recovers_after_remote_diverged() {
        let tmp = tempfile::tempdir().unwrap();
        let remote = init_bare(&tmp.path().join("remote.git"));
        let cache_a = tmp.path().join("cache-a");
        let cache_b = tmp.path().join("cache-b");

        // Machine A pushes v1; machine B pushes v2 on top.
        let staging_a = tmp.path().join("staging-a");
        write_staging(&staging_a, "v1");
        push_with_cache(&remote, &staging_a, &cache_a, "m").unwrap();
        let staging_b = tmp.path().join("staging-b");
        write_staging(&staging_b, "v2");
        push_with_cache(&remote, &staging_b, &cache_b, "m").unwrap();

        // A's cache is now behind a remote whose history it does not contain.
        // With ff-only pulling this wedged permanently; reset-based alignment
        // must recover and land v3.
        write_staging(&staging_a, "v3");
        push_with_cache(&remote, &staging_a, &cache_a, "m").unwrap();

        let pulled = tmp.path().join("pulled");
        pull_with_cache(&remote, &pulled, &tmp.path().join("cache-c"), None, "m").unwrap();
        assert_eq!(
            std::fs::read_to_string(pulled.join("data/settings.json")).unwrap(),
            "v3"
        );
    }

    #[test]
    fn refuses_ext_protocol_remote() {
        let tmp = tempfile::tempdir().unwrap();
        let staging = tmp.path().join("staging");
        write_staging(&staging, "{}");
        let marker = tmp.path().join("pwned");
        let remote = format!("ext::sh -c 'touch {}'", marker.display());
        let err = push_with_cache(&remote, &staging, &tmp.path().join("cache"), "m").unwrap_err();
        assert!(err.to_string().contains("git"), "got: {err:#}");
        assert!(!marker.exists(), "ext:: remote executed a command");
    }

    #[test]
    fn remote_override_repoints_existing_cache() {
        let tmp = tempfile::tempdir().unwrap();
        let remote_a = init_bare(&tmp.path().join("a.git"));
        let remote_b = init_bare(&tmp.path().join("b.git"));
        let cache = tmp.path().join("cache");

        let staging = tmp.path().join("staging");
        write_staging(&staging, "for-a");
        push_with_cache(&remote_a, &staging, &cache, "m").unwrap();

        // Same cache, different --remote: must land on B, not silently on A.
        write_staging(&staging, "for-b");
        push_with_cache(&remote_b, &staging, &cache, "m").unwrap();
        let pulled = tmp.path().join("pulled");
        pull_with_cache(&remote_b, &pulled, &tmp.path().join("cache-2"), None, "m").unwrap();
        assert_eq!(
            std::fs::read_to_string(pulled.join("data/settings.json")).unwrap(),
            "for-b"
        );
    }

    /// Add `rel` (under `data/`) with `content` to staging and its manifest.
    fn stage_file(staging: &Path, rel: &str, content: &[u8]) {
        use sha2::{Digest, Sha256};
        let path = staging.join("data").join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, content).unwrap();
        let mut m = Manifest::read_from(staging).unwrap();
        m.files.push(crate::manifest::FileEntry {
            rel_path: rel.to_string(),
            sha256: format!("{:x}", Sha256::digest(content)),
            size: content.len() as u64,
        });
        m.write_to(staging).unwrap();
    }

    #[test]
    fn nested_gitignore_and_crlf_files_still_roundtrip() {
        let tmp = tempfile::tempdir().unwrap();
        let remote = init_bare(&tmp.path().join("remote.git"));
        let staging = tmp.path().join("staging");
        write_staging(&staging, "{}");
        // A captured skill whose own ignore/attribute rules would drop or
        // rewrite files if git honored them inside the snapshot.
        stage_file(&staging, "skills/tool/.gitignore", b"*.log\nbuild/\n");
        stage_file(&staging, "skills/tool/.gitattributes", b"* text eol=lf\n");
        stage_file(&staging, "skills/tool/run.log", b"kept\n");
        stage_file(&staging, "skills/tool/build/out.txt", b"also kept\n");
        stage_file(&staging, "skills/tool/crlf.txt", b"a\r\nb\r\n");

        push_with_cache(&remote, &staging, &tmp.path().join("cache-a"), "m").unwrap();
        let pulled = tmp.path().join("pulled");
        pull_with_cache(&remote, &pulled, &tmp.path().join("cache-b"), None, "m").unwrap();

        let data = pulled.join("data/skills/tool");
        assert_eq!(std::fs::read(data.join("run.log")).unwrap(), b"kept\n");
        assert_eq!(
            std::fs::read(data.join("build/out.txt")).unwrap(),
            b"also kept\n"
        );
        assert_eq!(std::fs::read(data.join("crlf.txt")).unwrap(), b"a\r\nb\r\n");
        assert!(pulled.join("data/skills/tool/.gitignore").exists());
    }

    #[test]
    fn nested_git_dir_is_refused_with_path() {
        let tmp = tempfile::tempdir().unwrap();
        let remote = init_bare(&tmp.path().join("remote.git"));
        let staging = tmp.path().join("staging");
        write_staging(&staging, "{}");
        stage_file(&staging, "skills/tool/.git/HEAD", b"ref: refs/heads/main\n");
        let err = push_with_cache(&remote, &staging, &tmp.path().join("cache"), "m").unwrap_err();
        assert!(err.to_string().contains("skills/tool/.git"), "got: {err:#}");
    }

    #[test]
    fn switching_to_empty_remote_does_not_push_old_history() {
        let tmp = tempfile::tempdir().unwrap();
        let remote_a = init_bare(&tmp.path().join("a.git"));
        let remote_b = init_bare(&tmp.path().join("b.git"));
        let cache = tmp.path().join("cache");

        let staging = tmp.path().join("staging");
        write_staging(&staging, "a1");
        push_with_cache(&remote_a, &staging, &cache, "m").unwrap();
        write_staging(&staging, "a2");
        push_with_cache(&remote_a, &staging, &cache, "m").unwrap();

        write_staging(&staging, "for-b");
        push_with_cache(&remote_b, &staging, &cache, "m").unwrap();

        // B holds exactly one commit: none of A's history leaked into it.
        let count = run_git(
            &["rev-list", "--all", "--count"],
            Some(&tmp.path().join("b.git")),
        )
        .unwrap();
        assert_eq!(count.trim(), "1");
        let a_head = run_git(&["rev-parse", "HEAD"], Some(&tmp.path().join("a.git"))).unwrap();
        assert!(run_git(
            &["cat-file", "-e", a_head.trim()],
            Some(&tmp.path().join("b.git"))
        )
        .is_err());
    }

    #[test]
    fn read_paths_fail_instead_of_serving_stale_cache() {
        let tmp = tempfile::tempdir().unwrap();
        let remote_dir = tmp.path().join("remote.git");
        let remote = init_bare(&remote_dir);
        let staging = tmp.path().join("staging");
        write_staging(&staging, "v1");
        let cache = tmp.path().join("cache");
        push_with_cache(&remote, &staging, &cache, "m").unwrap();

        // The remote becomes unreachable: reads must error, not return v1.
        std::fs::remove_dir_all(&remote_dir).unwrap();
        let pulled = tmp.path().join("pulled");
        assert!(pull_with_cache(&remote, &pulled, &cache, None, "m").is_err());
        assert!(machines_with_cache(&remote, &cache).is_err());
        assert!(remote_manifest_with_cache(&remote, &cache, None, "m").is_err());
    }
}
