//! Git transport. The snapshot (manifest + `data/`) is mirrored into a local
//! clone of a remote repository and pushed; pulling does the inverse. We shell
//! out to the system `git` binary rather than linking libgit2 — it keeps the
//! build dependency-free and matches whatever auth (ssh keys, credential
//! helpers) the user already has configured.
//!
//! The local clone is cached under `~/.config/ccsync/repo` so subsequent
//! pushes/pulls are incremental.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result};
use walkdir::WalkDir;

use crate::error::CcError;
use crate::manifest::MANIFEST_NAME;

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

/// Replace the cache's manifest + data with the staged snapshot (so deletions
/// propagate) and commit if anything changed. A snapshot identical to what the
/// cache already holds commits nothing; the follow-up push is then a cheap
/// no-op against an up-to-date remote, or publishes the existing history to a
/// remote that does not have it yet.
fn overlay_and_commit(staging: &Path, cache: &Path) -> Result<()> {
    let _ = std::fs::remove_file(cache.join(MANIFEST_NAME));
    let _ = std::fs::remove_dir_all(cache.join("data"));
    copy_tree(&staging.join(MANIFEST_NAME), &cache.join(MANIFEST_NAME))?;
    let staged_data = staging.join("data");
    if staged_data.exists() {
        copy_tree(&staged_data, &cache.join("data"))?;
    }

    run_git(&["add", "-A"], Some(cache))?;
    let status = run_git(&["status", "--porcelain"], Some(cache))?;
    if status.trim().is_empty() {
        return Ok(());
    }
    let msg = format!("ccsync snapshot {}", chrono::Utc::now().to_rfc3339());
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

/// Push the staged snapshot to the configured git `remote`.
pub fn push(remote: &str, staging: &Path) -> Result<()> {
    let cache = repo_cache()?;
    push_with_cache(remote, staging, &cache)
}

pub(crate) fn push_with_cache(remote: &str, staging: &Path, cache: &Path) -> Result<()> {
    ensure_clone(remote, cache)?;
    overlay_and_commit(staging, cache)?;
    if run_git(&["push", "-u", "origin", "HEAD"], Some(cache)).is_ok() {
        return Ok(());
    }
    // Rejected — another machine pushed between our fetch and push. Re-align
    // to the new remote tip, re-overlay the snapshot, and retry exactly once;
    // a second rejection is surfaced to the caller.
    align_with_remote(cache);
    overlay_and_commit(staging, cache)?;
    run_git(&["push", "-u", "origin", "HEAD"], Some(cache))?;
    Ok(())
}

/// Pull the latest snapshot from `remote` into the `staging` directory.
pub fn pull(remote: &str, staging: &Path) -> Result<()> {
    let cache = repo_cache()?;
    pull_with_cache(remote, staging, &cache)
}

pub(crate) fn pull_with_cache(remote: &str, staging: &Path, cache: &Path) -> Result<()> {
    ensure_clone(remote, cache)?;

    if !cache.join(MANIFEST_NAME).exists() {
        return Err(CcError::NoStagedSnapshot(remote.to_string()).into());
    }

    if staging.exists() {
        std::fs::remove_dir_all(staging).ok();
    }
    std::fs::create_dir_all(staging)?;
    copy_tree(&cache.join(MANIFEST_NAME), &staging.join(MANIFEST_NAME))?;
    let repo_data = cache.join("data");
    if repo_data.exists() {
        copy_tree(&repo_data, &staging.join("data"))?;
    }
    Ok(())
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
    if !cache.join(".git").exists() {
        return Ok(Vec::new());
    }
    let n = limit.to_string();
    // A repo with no commits makes `git log` exit non-zero; treat that as empty.
    let out = match run_git(&["log", "--pretty=%h|%cI|%s", "-n", &n], Some(&cache)) {
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
        std::fs::write(staging.join(MANIFEST_NAME), r#"{"v":1}"#).unwrap();
        std::fs::write(staging.join("data/settings.json"), content).unwrap();
    }

    #[test]
    fn push_pull_roundtrip_via_bare_remote() {
        let tmp = tempfile::tempdir().unwrap();
        let remote = init_bare(&tmp.path().join("remote.git"));

        let staging = tmp.path().join("staging");
        write_staging(&staging, r#"{"theme":"dark"}"#);
        push_with_cache(&remote, &staging, &tmp.path().join("cache-a")).unwrap();

        // Pull through a different cache, as a second machine would.
        let pulled = tmp.path().join("pulled");
        pull_with_cache(&remote, &pulled, &tmp.path().join("cache-b")).unwrap();
        assert_eq!(
            std::fs::read_to_string(pulled.join("data/settings.json")).unwrap(),
            r#"{"theme":"dark"}"#
        );

        // Pushing the identical snapshot again is a no-op, not an error.
        push_with_cache(&remote, &staging, &tmp.path().join("cache-a")).unwrap();
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
        push_with_cache(&remote, &staging_a, &cache_a).unwrap();
        let staging_b = tmp.path().join("staging-b");
        write_staging(&staging_b, "v2");
        push_with_cache(&remote, &staging_b, &cache_b).unwrap();

        // A's cache is now behind a remote whose history it does not contain.
        // With ff-only pulling this wedged permanently; reset-based alignment
        // must recover and land v3.
        write_staging(&staging_a, "v3");
        push_with_cache(&remote, &staging_a, &cache_a).unwrap();

        let pulled = tmp.path().join("pulled");
        pull_with_cache(&remote, &pulled, &tmp.path().join("cache-c")).unwrap();
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
        let err = push_with_cache(&remote, &staging, &tmp.path().join("cache")).unwrap_err();
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
        push_with_cache(&remote_a, &staging, &cache).unwrap();

        // Same cache, different --remote: must land on B, not silently on A.
        write_staging(&staging, "for-b");
        push_with_cache(&remote_b, &staging, &cache).unwrap();
        let pulled = tmp.path().join("pulled");
        pull_with_cache(&remote_b, &pulled, &tmp.path().join("cache-2")).unwrap();
        assert_eq!(
            std::fs::read_to_string(pulled.join("data/settings.json")).unwrap(),
            "for-b"
        );
    }
}
