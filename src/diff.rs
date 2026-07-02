//! Comparing state trees. `ccsync diff` reports how the local `~/.claude`
//! differs from the staged snapshot without touching either; the same
//! tree-diff core backs `ccsync profile diff`.
//!
//! The local side is computed with a dry-run `snapshot::build`, so the
//! comparison sees exactly what a real snapshot would capture (include/
//! exclude rules, redaction, bundled MCP servers) rather than raw disk state.

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::Result;

use crate::config::Config;
use crate::manifest::Manifest;
use crate::snapshot::{self, SnapshotOptions};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiffState {
    /// Present locally, absent from the other side.
    LocalOnly,
    /// Absent locally, present on the other side.
    OtherOnly,
    /// Present on both sides with different content.
    Changed,
}

#[derive(Debug)]
pub struct DiffEntry {
    pub rel: String,
    pub state: DiffState,
}

/// Diff the local `~/.claude` (as a snapshot would capture it) against the
/// staged snapshot's manifest. Returns entries sorted by path; empty means
/// the two are identical. The local manifest is always computed dry-run so
/// staging is never touched.
pub fn against_staged(
    claude_dir: &Path,
    staging: &Path,
    config: &Config,
    claude_json: Option<std::path::PathBuf>,
) -> Result<Vec<DiffEntry>> {
    let staged = Manifest::read_from(staging)?;
    let opts = SnapshotOptions {
        dry_run: true,
        allow_secrets: false,
        claude_json,
        profiles_root: None,
    };
    let local = snapshot::build(claude_dir, staging, config, &opts)?;
    Ok(diff_manifest_maps(
        &hash_map_of(&local),
        &hash_map_of(&staged),
    ))
}

fn hash_map_of(m: &Manifest) -> BTreeMap<String, String> {
    m.files
        .iter()
        .map(|f| (f.rel_path.clone(), f.sha256.clone()))
        .collect()
}

fn diff_manifest_maps(
    local: &BTreeMap<String, String>,
    other: &BTreeMap<String, String>,
) -> Vec<DiffEntry> {
    let mut out = Vec::new();
    for (rel, sha) in local {
        match other.get(rel) {
            None => out.push(DiffEntry {
                rel: rel.clone(),
                state: DiffState::LocalOnly,
            }),
            Some(other_sha) if other_sha != sha => out.push(DiffEntry {
                rel: rel.clone(),
                state: DiffState::Changed,
            }),
            _ => {}
        }
    }
    for rel in other.keys() {
        if !local.contains_key(rel) {
            out.push(DiffEntry {
                rel: rel.clone(),
                state: DiffState::OtherOnly,
            });
        }
    }
    out.sort_by(|a, b| a.rel.cmp(&b.rel));
    out
}

/// Diff two plain file trees by content hash. `LocalOnly` means present under
/// `local` only; `OtherOnly` present under `other` only. Used by
/// `ccsync profile diff` to compare `~/.claude` components against a profile
/// store.
pub fn diff_trees(local: &Path, other: &Path) -> Result<Vec<DiffEntry>> {
    Ok(diff_manifest_maps(
        &tree_hashes(local)?,
        &tree_hashes(other)?,
    ))
}

fn tree_hashes(root: &Path) -> Result<BTreeMap<String, String>> {
    use sha2::{Digest, Sha256};

    let mut out = BTreeMap::new();
    if !root.exists() {
        return Ok(out);
    }
    for entry in walkdir::WalkDir::new(root).follow_links(false) {
        let entry = entry?;
        if !entry.file_type().is_file() {
            continue;
        }
        let rel = entry
            .path()
            .strip_prefix(root)
            .expect("walked path is under root")
            .to_string_lossy()
            .replace('\\', "/");
        let bytes = std::fs::read(entry.path())?;
        out.insert(rel, snapshot::hex(&Sha256::digest(&bytes)));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(path: &Path, content: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, content).unwrap();
    }

    #[test]
    fn diff_trees_reports_all_three_states() {
        let tmp = tempfile::tempdir().unwrap();
        let a = tmp.path().join("a");
        let b = tmp.path().join("b");
        write(&a.join("x/f.md"), "same");
        write(&b.join("x/f.md"), "same");
        assert!(diff_trees(&a, &b).unwrap().is_empty());

        write(&a.join("x/f.md"), "changed");
        write(&a.join("only-local.md"), "l");
        write(&b.join("only-other.md"), "o");
        let entries = diff_trees(&a, &b).unwrap();
        let find = |rel: &str| entries.iter().find(|e| e.rel == rel).unwrap();
        assert_eq!(find("x/f.md").state, DiffState::Changed);
        assert_eq!(find("only-local.md").state, DiffState::LocalOnly);
        assert_eq!(find("only-other.md").state, DiffState::OtherOnly);
    }

    #[test]
    fn diffs_local_against_staged_manifest() {
        let tmp = tempfile::tempdir().unwrap();
        let claude = tmp.path().join("claude");
        let staging = tmp.path().join("staging");

        // Stage a snapshot of the original state.
        write(&claude.join("settings.json"), r#"{"theme":"dark"}"#);
        write(&claude.join("CLAUDE.md"), "# memory");
        let cfg = Config::default();
        let opts = SnapshotOptions {
            dry_run: false,
            allow_secrets: false,
            claude_json: None,
            profiles_root: None,
        };
        snapshot::build(&claude, &staging, &cfg, &opts).unwrap();

        // Mutate local state: change one file, add one, remove one.
        write(&claude.join("settings.json"), r#"{"theme":"light"}"#);
        write(&claude.join("skills/new/SKILL.md"), "# new");
        std::fs::remove_file(claude.join("CLAUDE.md")).unwrap();

        let entries = against_staged(&claude, &staging, &cfg, None).unwrap();
        let find = |rel: &str| entries.iter().find(|e| e.rel == rel).unwrap();
        assert_eq!(find("settings.json").state, DiffState::Changed);
        assert_eq!(find("skills/new/SKILL.md").state, DiffState::LocalOnly);
        assert_eq!(find("CLAUDE.md").state, DiffState::OtherOnly);
        assert_eq!(entries.len(), 3);
    }
}
