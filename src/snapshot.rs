//! Building a snapshot: walk `~/.claude`, apply the include/exclude rules,
//! hard-block credentials, optionally scan text configs for secrets, copy the
//! surviving files into the staging `data/` directory, and write a manifest.
//!
//! Staging layout:
//! ```text
//! <staging>/
//! ├── manifest.json
//! └── data/                # mirrors the relevant subtree of ~/.claude
//!     ├── settings.json
//!     ├── CLAUDE.md
//!     └── projects/-home-user-x/session.jsonl
//! ```

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use sha2::{Digest, Sha256};
use walkdir::WalkDir;

use crate::config::Config;
use crate::error::CcError;
use crate::manifest::{FileEntry, Manifest, ProjectRoot};
use crate::paths;
use crate::redact;

pub struct SnapshotOptions {
    pub dry_run: bool,
    pub allow_secrets: bool,
}

/// File extensions we treat as text and therefore scan for secrets.
const SCANNED_EXTS: &[&str] = &["json", "toml", "md", "yaml", "yml", "env"];

/// Build a snapshot of `claude_dir` into `staging`. Returns the manifest.
pub fn build(
    claude_dir: &Path,
    staging: &Path,
    config: &Config,
    opts: &SnapshotOptions,
) -> Result<Manifest> {
    let host = hostname();
    let home = paths::home_dir()?.to_string_lossy().to_string();
    let mut manifest = Manifest::new(host, home);

    let data_root = staging.join("data");
    if !opts.dry_run {
        // Start from a clean staging data dir so removed files don't linger.
        if data_root.exists() {
            fs::remove_dir_all(&data_root)
                .with_context(|| format!("clearing staging dir {}", data_root.display()))?;
        }
        fs::create_dir_all(&data_root)?;
    }

    // Resolve which top-level include entries actually apply.
    for entry in &config.include {
        if entry == "projects" && !config.include_sessions {
            continue;
        }
        let src = claude_dir.join(entry);
        if !src.exists() {
            continue;
        }
        capture_path(&src, claude_dir, &data_root, config, opts, &mut manifest)?;
    }

    // Record decoded project roots for remapping, even in dry-run.
    if config.include_sessions {
        let projects = claude_dir.join("projects");
        if projects.is_dir() {
            for child in fs::read_dir(&projects)? {
                let child = child?;
                if child.file_type()?.is_dir() {
                    let encoded = child.file_name().to_string_lossy().to_string();
                    manifest.project_roots.push(ProjectRoot {
                        decoded_path: paths::decode_path(&encoded).to_string_lossy().to_string(),
                        encoded,
                    });
                }
            }
        }
    }

    if !opts.dry_run {
        manifest.write_to(staging)?;
    }
    Ok(manifest)
}

/// Capture a single include entry, which may be a file or a directory tree.
fn capture_path(
    src: &Path,
    claude_dir: &Path,
    data_root: &Path,
    config: &Config,
    opts: &SnapshotOptions,
    manifest: &mut Manifest,
) -> Result<()> {
    for entry in WalkDir::new(src).follow_links(false) {
        let entry = entry?;
        if !entry.file_type().is_file() {
            continue;
        }
        let abs = entry.path();
        let rel = abs
            .strip_prefix(claude_dir)
            .expect("walked path is under claude_dir")
            .to_string_lossy()
            .replace('\\', "/");

        let file_name = abs
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_default();

        // Hard block: credentials never leave the machine.
        if redact::is_credential_file(&file_name) {
            return Err(CcError::CredentialBlocked(rel).into());
        }

        if config.is_excluded(&rel) {
            continue;
        }

        // Secret scan for text configs unless explicitly allowed.
        if !opts.allow_secrets && is_scanned(abs) {
            if let Ok(text) = fs::read_to_string(abs) {
                if let Some(hint) = redact::scan_for_secrets(&text) {
                    return Err(CcError::SecretDetected { file: rel, hint }.into());
                }
            }
        }

        let bytes = fs::read(abs).with_context(|| format!("reading {}", abs.display()))?;
        let sha256 = hex(&Sha256::digest(&bytes));
        manifest.files.push(FileEntry {
            rel_path: rel.clone(),
            sha256,
            size: bytes.len() as u64,
        });

        if !opts.dry_run {
            let dest = data_root.join(&rel);
            if let Some(parent) = dest.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::write(&dest, &bytes).with_context(|| format!("writing {}", dest.display()))?;
        }
    }
    Ok(())
}

fn is_scanned(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| SCANNED_EXTS.contains(&e.to_ascii_lowercase().as_str()))
        .unwrap_or(false)
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(s, "{b:02x}");
    }
    s
}

fn hostname() -> String {
    std::env::var("HOSTNAME")
        .or_else(|_| std::env::var("COMPUTERNAME"))
        .unwrap_or_else(|_| "unknown".to_string())
}

/// Convenience used by `restore`/`pull` to confirm a staged snapshot exists.
pub fn require_staged(staging: &Path) -> Result<PathBuf> {
    let data = staging.join("data");
    if !staging.join(crate::manifest::MANIFEST_NAME).exists() || !data.exists() {
        return Err(CcError::NoStagedSnapshot(staging.display().to_string()).into());
    }
    Ok(data)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(path: &Path, content: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
    }

    #[test]
    fn captures_includes_and_skips_excludes() {
        let tmp = tempfile::tempdir().unwrap();
        let claude = tmp.path().join("claude");
        let staging = tmp.path().join("staging");
        write(&claude.join("settings.json"), r#"{"theme":"dark"}"#);
        write(&claude.join("CLAUDE.md"), "# memory");
        write(&claude.join("shell-snapshots/snap.sh"), "echo hi");
        write(
            &claude.join("projects/-home-alice-proj/sess.jsonl"),
            "{\"cwd\":\"/home/alice/proj\"}\n",
        );

        let cfg = Config::default();
        let opts = SnapshotOptions { dry_run: false, allow_secrets: false };
        let m = build(&claude, &staging, &cfg, &opts).unwrap();

        let rels: Vec<&str> = m.files.iter().map(|f| f.rel_path.as_str()).collect();
        assert!(rels.contains(&"settings.json"));
        assert!(rels.contains(&"CLAUDE.md"));
        assert!(rels.contains(&"projects/-home-alice-proj/sess.jsonl"));
        // Excluded machine-local state is not captured.
        assert!(!rels.iter().any(|r| r.starts_with("shell-snapshots")));
        // Project roots recorded for remapping.
        assert_eq!(m.project_roots.len(), 1);
        assert_eq!(m.project_roots[0].decoded_path, "/home/alice/proj");
        // Files actually landed in staging.
        assert!(staging.join("data/settings.json").exists());
    }

    #[test]
    fn hard_blocks_credentials() {
        let tmp = tempfile::tempdir().unwrap();
        let claude = tmp.path().join("claude");
        let staging = tmp.path().join("staging");
        write(&claude.join("settings.json"), "{}");
        write(&claude.join(".credentials.json"), r#"{"token":"x"}"#);
        // Add .credentials.json to includes to prove the hard block wins.
        let mut cfg = Config::default();
        cfg.include.push(".credentials.json".into());
        cfg.exclude.clear();
        let opts = SnapshotOptions { dry_run: false, allow_secrets: true };
        let err = build(&claude, &staging, &cfg, &opts).unwrap_err();
        assert!(err.to_string().contains("credential"));
    }

    #[test]
    fn aborts_on_secret_in_settings() {
        let tmp = tempfile::tempdir().unwrap();
        let claude = tmp.path().join("claude");
        let staging = tmp.path().join("staging");
        write(
            &claude.join("settings.json"),
            r#"{"env":{"ANTHROPIC_API_KEY":"sk-abcdefghijklmnopqrstuvwx"}}"#,
        );
        let cfg = Config::default();
        let opts = SnapshotOptions { dry_run: false, allow_secrets: false };
        let err = build(&claude, &staging, &cfg, &opts).unwrap_err();
        assert!(err.to_string().contains("secret"));

        // With allow_secrets it succeeds.
        let opts = SnapshotOptions { dry_run: false, allow_secrets: true };
        assert!(build(&claude, &staging, &cfg, &opts).is_ok());
    }
}
