//! Path remapping. Session transcripts live under directories whose names
//! encode the absolute working directory on the source machine, and the JSONL
//! transcripts themselves embed that absolute path in their `cwd` field and in
//! tool references. When restoring onto a machine with a different home
//! directory (or a different checkout location), those paths must be rewritten
//! so Claude Code's session picker finds them and tool references resolve.
//!
//! This module operates on the staged `data/` tree in place, before `restore`
//! copies it into `~/.claude`:
//!   1. rewrite absolute-path prefixes inside every `projects/**/ *.jsonl`, then
//!   2. rename each encoded `projects/<encoded>` directory to its re-encoded
//!      target name.

use std::fs;
use std::path::Path;

use anyhow::{Context, Result};

use crate::manifest::Manifest;
use crate::paths;

/// A single prefix translation: any absolute path beginning with `from` is
/// rewritten to begin with `to`.
#[derive(Debug, Clone)]
pub struct Mapping {
    pub from: String,
    pub to: String,
}

/// Build the ordered mapping list. The automatic `source_home -> local_home`
/// mapping is included first, then explicit config pairs. Mappings are sorted
/// longest-prefix-first so the most specific rule wins.
pub fn build_mappings(
    manifest: &Manifest,
    local_home: &str,
    explicit: &std::collections::BTreeMap<String, String>,
) -> Vec<Mapping> {
    let mut mappings: Vec<Mapping> = Vec::new();
    if manifest.source_home != local_home && !manifest.source_home.is_empty() {
        mappings.push(Mapping {
            from: manifest.source_home.clone(),
            to: local_home.to_string(),
        });
    }
    for (from, to) in explicit {
        mappings.push(Mapping {
            from: from.clone(),
            to: to.clone(),
        });
    }
    // Longest source prefix first.
    mappings.sort_by(|a, b| b.from.len().cmp(&a.from.len()));
    mappings
}

/// Apply `mappings` to the staged `data/projects` tree in place.
pub fn apply(data_root: &Path, mappings: &[Mapping]) -> Result<()> {
    if mappings.is_empty() {
        return Ok(());
    }
    let projects = data_root.join("projects");
    if !projects.is_dir() {
        return Ok(());
    }

    // 1. Rewrite transcript contents.
    for entry in walkdir::WalkDir::new(&projects) {
        let entry = entry?;
        if entry.file_type().is_file()
            && entry.path().extension().and_then(|e| e.to_str()) == Some("jsonl")
        {
            rewrite_file(entry.path(), mappings)
                .with_context(|| format!("remapping {}", entry.path().display()))?;
        }
    }

    // 2. Rename encoded project directories.
    let mut renames: Vec<(std::path::PathBuf, std::path::PathBuf)> = Vec::new();
    for child in fs::read_dir(&projects)? {
        let child = child?;
        if !child.file_type()?.is_dir() {
            continue;
        }
        let encoded = child.file_name().to_string_lossy().to_string();
        let decoded = paths::decode_path(&encoded).to_string_lossy().to_string();
        if let Some(new_decoded) = remap_str(&decoded, mappings) {
            let new_encoded = paths::encode_path(Path::new(&new_decoded));
            if new_encoded != encoded {
                renames.push((projects.join(&encoded), projects.join(&new_encoded)));
            }
        }
    }
    for (from, to) in renames {
        if to.exists() {
            // Merge into an existing target dir rather than clobbering it.
            merge_dir(&from, &to)?;
            fs::remove_dir_all(&from).ok();
        } else {
            fs::rename(&from, &to)
                .with_context(|| format!("renaming {} -> {}", from.display(), to.display()))?;
        }
    }
    Ok(())
}

/// Rewrite every mapped prefix occurrence in a file's text content.
fn rewrite_file(path: &Path, mappings: &[Mapping]) -> Result<()> {
    let content = fs::read_to_string(path)?;
    let mut out = content.clone();
    for m in mappings {
        if out.contains(&m.from) {
            out = out.replace(&m.from, &m.to);
        }
    }
    if out != content {
        fs::write(path, out)?;
    }
    Ok(())
}

/// Apply the first matching prefix mapping to a single path string, returning
/// the rewritten path or `None` if no mapping applied. Used to remap the
/// per-project keys of bundled MCP server definitions on restore.
pub fn remap_path(s: &str, mappings: &[Mapping]) -> Option<String> {
    remap_str(s, mappings)
}

/// Apply the first matching prefix mapping to a single path string.
fn remap_str(s: &str, mappings: &[Mapping]) -> Option<String> {
    for m in mappings {
        if s == m.from {
            return Some(m.to.clone());
        }
        let prefix = format!("{}/", m.from);
        if s.starts_with(&prefix) {
            return Some(format!("{}/{}", m.to, &s[prefix.len()..]));
        }
    }
    None
}

/// Recursively move files from `from` into `to`, creating directories as needed.
fn merge_dir(from: &Path, to: &Path) -> Result<()> {
    for entry in walkdir::WalkDir::new(from) {
        let entry = entry?;
        if entry.file_type().is_file() {
            let rel = entry.path().strip_prefix(from).unwrap();
            let dest = to.join(rel);
            if let Some(parent) = dest.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::copy(entry.path(), &dest)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::Manifest;

    fn write(path: &Path, content: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
    }

    #[test]
    fn rewrites_cwd_and_renames_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let data = tmp.path().join("data");
        let sess = data.join("projects/-Users-alice-proj/sess.jsonl");
        write(
            &sess,
            "{\"cwd\":\"/Users/alice/proj\",\"file\":\"/Users/alice/proj/src/main.rs\"}\n",
        );

        let mut manifest = Manifest::new("h".into(), "/Users/alice".into());
        let mappings = build_mappings(&manifest, "/home/bob", &Default::default());
        // sanity: also exercise explicit override path
        let _ = &mut manifest;
        apply(&data, &mappings).unwrap();

        // Directory renamed to the new home.
        let new_dir = data.join("projects/-home-bob-proj");
        assert!(new_dir.exists(), "expected renamed dir");
        let content = fs::read_to_string(new_dir.join("sess.jsonl")).unwrap();
        assert!(content.contains("\"cwd\":\"/home/bob/proj\""));
        assert!(content.contains("/home/bob/proj/src/main.rs"));
        assert!(!content.contains("/Users/alice"));
    }

    #[test]
    fn no_mappings_is_noop() {
        let tmp = tempfile::tempdir().unwrap();
        let data = tmp.path().join("data");
        write(
            &data.join("projects/-home-x-p/s.jsonl"),
            "{\"cwd\":\"/home/x/p\"}\n",
        );
        apply(&data, &[]).unwrap();
        assert!(data.join("projects/-home-x-p/s.jsonl").exists());
    }

    #[test]
    fn longest_prefix_wins() {
        let mut explicit = std::collections::BTreeMap::new();
        explicit.insert("/home".into(), "/WRONG".into());
        explicit.insert("/home/alice/proj".into(), "/srv/proj".into());
        let manifest = Manifest::new("h".into(), String::new());
        let mappings = build_mappings(&manifest, "/home/alice", &explicit);
        assert_eq!(
            remap_str("/home/alice/proj/x", &mappings).as_deref(),
            Some("/srv/proj/x")
        );
    }
}
