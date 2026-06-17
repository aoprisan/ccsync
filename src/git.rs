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

/// Ensure the local cache is a clone of `remote`, pulling latest if it already
/// exists.
fn ensure_clone(remote: &str, cache: &Path) -> Result<()> {
    if cache.join(".git").exists() {
        // Best-effort fast-forward; a brand-new remote may have no commits yet.
        let _ = run_git(&["fetch", "origin"], Some(cache));
        let _ = run_git(&["pull", "--ff-only"], Some(cache));
    } else {
        if let Some(parent) = cache.parent() {
            std::fs::create_dir_all(parent)?;
        }
        run_git(&["clone", remote, &cache.to_string_lossy()], None)?;
    }
    Ok(())
}

/// Push the staged snapshot to the configured git `remote`.
pub fn push(remote: &str, staging: &Path) -> Result<()> {
    let cache = repo_cache()?;
    ensure_clone(remote, &cache)?;

    // Replace the repo's manifest + data with the staged snapshot so deletions
    // propagate.
    let _ = std::fs::remove_file(cache.join(MANIFEST_NAME));
    let _ = std::fs::remove_dir_all(cache.join("data"));
    copy_tree(&staging.join(MANIFEST_NAME), &cache.join(MANIFEST_NAME))?;
    let staged_data = staging.join("data");
    if staged_data.exists() {
        copy_tree(&staged_data, &cache.join("data"))?;
    }

    run_git(&["add", "-A"], Some(&cache))?;
    // Nothing to commit is not an error.
    let status = run_git(&["status", "--porcelain"], Some(&cache))?;
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
        Some(&cache),
    )?;
    run_git(&["push", "-u", "origin", "HEAD"], Some(&cache))?;
    Ok(())
}

/// Pull the latest snapshot from `remote` into the `staging` directory.
pub fn pull(remote: &str, staging: &Path) -> Result<()> {
    let cache = repo_cache()?;
    ensure_clone(remote, &cache)?;

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
    for entry in WalkDir::new(src) {
        let entry = entry?;
        if entry.file_type().is_file() {
            let rel = entry.path().strip_prefix(src).unwrap();
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
