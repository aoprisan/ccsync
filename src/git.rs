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

/// Ensure the local cache is a clone of `remote`, aligned with the remote tip
/// if it already exists.
fn ensure_clone(remote: &str, cache: &Path) -> Result<()> {
    if cache.join(".git").exists() {
        // The cache may have been cloned from a different remote; keep origin
        // pointed at what the caller asked for so a --remote override is
        // honored instead of silently syncing with the old URL.
        run_git(&["remote", "set-url", "origin", remote], Some(cache))?;
        align_with_remote(cache);
    } else {
        if let Some(parent) = cache.parent() {
            std::fs::create_dir_all(parent)?;
        }
        run_git(&["clone", "--", remote, &cache.to_string_lossy()], None)?;
    }
    Ok(())
}

/// Best-effort: move the cache's branch to the remote tip. Snapshots replace
/// the whole repo state on every push (last writer wins), so the cache's own
/// history is disposable — `reset --hard` instead of merge/ff means a cache
/// that diverged from the remote (two machines pushing) can always recover.
/// Errors are ignored: a brand-new remote has no commits yet, and offline
/// operation should still be able to stage local commits.
fn align_with_remote(cache: &Path) {
    let _ = run_git(&["fetch", "origin"], Some(cache));
    if let Ok(branch) = run_git(&["symbolic-ref", "--short", "HEAD"], Some(cache)) {
        let remote_ref = format!("origin/{}", branch.trim());
        if run_git(&["rev-parse", "--verify", &remote_ref], Some(cache)).is_ok() {
            let _ = run_git(&["reset", "--hard", &remote_ref], Some(cache));
        }
    }
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
    migrate_legacy_root(cache)?;

    let subtree = cache.join(MACHINES_DIR).join(machine_id);
    let _ = std::fs::remove_dir_all(&subtree);
    copy_tree(&staging.join(MANIFEST_NAME), &subtree.join(MANIFEST_NAME))?;
    let staged_data = staging.join("data");
    if staged_data.exists() {
        copy_tree(&staged_data, &subtree.join("data"))?;
    }

    run_git(&["add", "-A"], Some(cache))?;
    let status = run_git(&["status", "--porcelain"], Some(cache))?;
    if status.trim().is_empty() {
        return Ok(());
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
            "-m",
            &msg,
        ],
        Some(cache),
    )?;
    Ok(())
}

/// Push the staged snapshot to this machine's subtree on the git `remote`.
pub fn push(remote: &str, staging: &Path, machine_id: &str) -> Result<()> {
    let cache = repo_cache()?;
    push_with_cache(remote, staging, &cache, machine_id)
}

pub(crate) fn push_with_cache(
    remote: &str,
    staging: &Path,
    cache: &Path,
    machine_id: &str,
) -> Result<()> {
    ensure_clone(remote, cache)?;
    overlay_and_commit(staging, cache, machine_id)?;
    if run_git(&["push", "-u", "origin", "HEAD"], Some(cache)).is_ok() {
        return Ok(());
    }
    // Rejected — another machine pushed between our fetch and push. Re-align
    // to the new remote tip, re-overlay the snapshot, and retry exactly once;
    // a second rejection is surfaced to the caller. Each machine writes only
    // its own subtree, so the re-overlay cannot lose the other machine's push.
    align_with_remote(cache);
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
    let cache = repo_cache()?;
    pull_with_cache(remote, staging, &cache, from, own_id)
}

pub(crate) fn pull_with_cache(
    remote: &str,
    staging: &Path,
    cache: &Path,
    from: Option<&str>,
    own_id: &str,
) -> Result<()> {
    ensure_clone(remote, cache)?;
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
    let cache = repo_cache()?;
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
    ensure_clone(remote, cache)?;
    run_git(&["rev-parse", "--verify", "--quiet", commit], Some(cache))
        .map_err(|_| anyhow::anyhow!("commit {commit} not found; see `ccsync history`"))?;
    run_git(&["reset", "--hard", commit], Some(cache))?;
    let result =
        select_subtree(cache, from, own_id).and_then(|subtree| copy_snapshot(&subtree, staging));
    // Whatever happened, put the cache back on the remote tip.
    align_with_remote(cache);
    result
}

/// Materialize the snapshot in `subtree` into a fresh `staging`.
fn copy_snapshot(subtree: &Path, staging: &Path) -> Result<()> {
    if staging.exists() {
        std::fs::remove_dir_all(staging).ok();
    }
    std::fs::create_dir_all(staging)?;
    copy_tree(&subtree.join(MANIFEST_NAME), &staging.join(MANIFEST_NAME))?;
    let repo_data = subtree.join("data");
    if repo_data.exists() {
        copy_tree(&repo_data, &staging.join("data"))?;
    }
    Ok(())
}

/// Refresh the local cache from `remote` (cloning it if needed) without
/// copying anything into staging. Used before reading history/manifests.
pub fn refresh_cache(remote: &str) -> Result<()> {
    let cache = repo_cache()?;
    ensure_clone(remote, &cache)
}

/// Every machine with a snapshot on the remote, with its manifest (source
/// host, timestamp, file count, producing version).
pub fn machines(remote: &str) -> Result<Vec<(String, Manifest)>> {
    let cache = repo_cache()?;
    machines_with_cache(remote, &cache)
}

pub(crate) fn machines_with_cache(remote: &str, cache: &Path) -> Result<Vec<(String, Manifest)>> {
    ensure_clone(remote, cache)?;
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
    let cache = repo_cache()?;
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
}
