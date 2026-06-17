//! A unified, read-only view of the local backups that ccsync knows about.
//!
//! Snapshots are not kept in any single place: the current staged snapshot
//! lives under `~/.config/ccsync/staging`, versioned snapshots accumulate as
//! commits in the local repo cache (`~/.config/ccsync/repo`), managed encrypted
//! archives are written into `~/.config/ccsync/backups`, and `restore` leaves
//! timestamped `~/.claude.ccsync-backup-*` copies next to `~/.claude`. This
//! module gathers all four into one list for the TUI to display.

use std::path::PathBuf;

use chrono::{DateTime, Local};

use crate::config::Config;
use crate::git;
use crate::manifest::Manifest;
use crate::paths;

/// Where a backup came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackupKind {
    /// The snapshot currently materialized in the staging directory.
    Staged,
    /// A commit in the local git repo cache.
    GitCommit,
    /// A managed encrypted archive under `~/.config/ccsync/backups`.
    Archive,
    /// A timestamped `~/.claude.ccsync-backup-*` left behind by `restore`.
    RestoreBackup,
}

impl BackupKind {
    /// Short human label used as a column in the TUI list.
    pub fn label(self) -> &'static str {
        match self {
            BackupKind::Staged => "staged",
            BackupKind::GitCommit => "git",
            BackupKind::Archive => "archive",
            BackupKind::RestoreBackup => "restore-bak",
        }
    }
}

/// One row in the backups list.
#[derive(Debug, Clone)]
pub struct LocalBackup {
    pub kind: BackupKind,
    /// Identifier: commit short-hash, archive filename, or directory name.
    pub label: String,
    /// Creation/commit time, when known.
    pub created_at: Option<String>,
    /// Free-form detail: host, file count, size, commit subject, etc.
    pub detail: String,
    // Carried as part of the backup model for callers and future actions
    // (e.g. open/delete a selected backup); not every field is rendered today.
    #[allow(dead_code)]
    pub size: Option<u64>,
    #[allow(dead_code)]
    pub path: Option<PathBuf>,
}

/// Gather every local backup ccsync can find. Each source is best-effort: a
/// missing or unreadable source contributes nothing rather than failing.
pub fn collect(_config: &Config) -> Vec<LocalBackup> {
    let mut out = Vec::new();
    collect_staged(&mut out);
    collect_git(&mut out);
    collect_archives(&mut out);
    collect_restore_backups(&mut out);
    out
}

fn collect_staged(out: &mut Vec<LocalBackup>) {
    let Ok(staging) = paths::staging_dir() else {
        return;
    };
    let Ok(manifest) = Manifest::read_from(&staging) else {
        return;
    };
    let size: u64 = manifest.files.iter().map(|f| f.size).sum();
    out.push(LocalBackup {
        kind: BackupKind::Staged,
        label: "current staging".to_string(),
        created_at: Some(manifest.created_at.clone()),
        detail: format!(
            "host {} · {} files · {}",
            manifest.source_host,
            manifest.files.len(),
            human_size(size),
        ),
        size: Some(size),
        path: Some(staging),
    });
}

fn collect_git(out: &mut Vec<LocalBackup>) {
    let commits = match git::log(50) {
        Ok(c) => c,
        Err(_) => return,
    };
    for (hash, date, subject) in commits {
        out.push(LocalBackup {
            kind: BackupKind::GitCommit,
            label: hash,
            created_at: if date.is_empty() { None } else { Some(date) },
            detail: subject,
            size: None,
            path: None,
        });
    }
}

fn collect_archives(out: &mut Vec<LocalBackup>) {
    let Ok(dir) = paths::backups_dir() else {
        return;
    };
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return;
    };
    let mut archives: Vec<LocalBackup> = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().to_string();
        if !name.ends_with(".age") {
            continue;
        }
        let meta = match entry.metadata() {
            Ok(m) if m.is_file() => m,
            _ => continue,
        };
        let created_at = meta.modified().ok().map(format_mtime);
        archives.push(LocalBackup {
            kind: BackupKind::Archive,
            label: name,
            created_at,
            detail: human_size(meta.len()),
            size: Some(meta.len()),
            path: Some(path),
        });
    }
    // Newest first.
    archives.sort_by(|a, b| b.created_at.cmp(&a.created_at));
    out.extend(archives);
}

fn collect_restore_backups(out: &mut Vec<LocalBackup>) {
    let Ok(claude) = paths::claude_dir() else {
        return;
    };
    let Some(parent) = claude.parent().map(|p| p.to_path_buf()) else {
        return;
    };
    let base = claude
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| ".claude".to_string());
    let prefix = format!("{base}.ccsync-backup-");
    let Ok(entries) = std::fs::read_dir(&parent) else {
        return;
    };
    let mut backups: Vec<LocalBackup> = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if !name.starts_with(&prefix) {
            continue;
        }
        let meta = match entry.metadata() {
            Ok(m) if m.is_dir() => m,
            _ => continue,
        };
        let created_at = meta.modified().ok().map(format_mtime);
        backups.push(LocalBackup {
            kind: BackupKind::RestoreBackup,
            label: name,
            created_at,
            detail: "pre-restore copy of ~/.claude".to_string(),
            size: None,
            path: Some(entry.path()),
        });
    }
    backups.sort_by(|a, b| b.label.cmp(&a.label));
    out.extend(backups);
}

fn format_mtime(t: std::time::SystemTime) -> String {
    let dt: DateTime<Local> = t.into();
    dt.format("%Y-%m-%d %H:%M:%S").to_string()
}

/// Format a byte count as a compact human-readable string.
pub fn human_size(bytes: u64) -> String {
    const UNITS: &[&str] = &["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::FileEntry;

    fn with_config_dir<T>(dir: &std::path::Path, f: impl FnOnce() -> T) -> T {
        // `paths::*_dir()` derive from the config dir, which `dirs` reads from
        // XDG_CONFIG_HOME on Linux. Point it at a temp dir for the test.
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
    fn staged_snapshot_is_listed() {
        let tmp = tempfile::tempdir().unwrap();
        with_config_dir(tmp.path(), || {
            let staging = paths::staging_dir().unwrap();
            std::fs::create_dir_all(&staging).unwrap();
            let mut m = Manifest::new("host1".into(), "/home/alice".into());
            m.files.push(FileEntry {
                rel_path: "settings.json".into(),
                sha256: "abc".into(),
                size: 100,
            });
            m.files.push(FileEntry {
                rel_path: "CLAUDE.md".into(),
                sha256: "def".into(),
                size: 28,
            });
            m.write_to(&staging).unwrap();

            let mut out = Vec::new();
            collect_staged(&mut out);
            assert_eq!(out.len(), 1);
            assert_eq!(out[0].kind, BackupKind::Staged);
            assert_eq!(out[0].size, Some(128));
            assert!(out[0].detail.contains("2 files"));
            assert!(out[0].detail.contains("host1"));
        });
    }

    #[test]
    fn managed_archives_are_listed() {
        let tmp = tempfile::tempdir().unwrap();
        with_config_dir(tmp.path(), || {
            let dir = paths::backups_dir().unwrap();
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("claude-backup-1.tar.gz.age"), b"ciphertext").unwrap();
            // A non-archive file must be ignored.
            std::fs::write(dir.join("notes.txt"), b"ignore me").unwrap();

            let mut out = Vec::new();
            collect_archives(&mut out);
            assert_eq!(out.len(), 1);
            assert_eq!(out[0].kind, BackupKind::Archive);
            assert_eq!(out[0].label, "claude-backup-1.tar.gz.age");
            assert_eq!(out[0].size, Some(10));
        });
    }

    #[test]
    fn restore_backups_are_listed() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        let claude = home.join(".claude");
        std::fs::create_dir_all(&claude).unwrap();
        std::fs::create_dir_all(home.join(".claude.ccsync-backup-20260101-101010")).unwrap();
        // Unrelated sibling dir must be ignored.
        std::fs::create_dir_all(home.join(".config")).unwrap();

        let prev_claude = std::env::var("CLAUDE_CONFIG_DIR").ok();
        std::env::set_var("CLAUDE_CONFIG_DIR", &claude);
        let mut out = Vec::new();
        collect_restore_backups(&mut out);
        match prev_claude {
            Some(p) => std::env::set_var("CLAUDE_CONFIG_DIR", p),
            None => std::env::remove_var("CLAUDE_CONFIG_DIR"),
        }

        assert_eq!(out.len(), 1);
        assert_eq!(out[0].kind, BackupKind::RestoreBackup);
        assert_eq!(out[0].label, ".claude.ccsync-backup-20260101-101010");
    }

    #[test]
    fn human_size_scales() {
        assert_eq!(human_size(512), "512 B");
        assert_eq!(human_size(2048), "2.0 KiB");
        assert_eq!(human_size(5 * 1024 * 1024), "5.0 MiB");
    }
}
