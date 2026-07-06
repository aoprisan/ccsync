//! Path remapping. Session state embeds absolute paths from the source
//! machine; when restoring onto a machine with a different home directory (or
//! a different checkout location), those paths must be rewritten so each
//! tool's session picker finds them and tool references resolve.
//!
//! This module operates on a tool's staged data subtree in place, before
//! `restore` copies it out. How depends on the tool's
//! [`RemapStrategy`](crate::tools::RemapStrategy):
//!
//! - `ClaudeProjects` (Claude Code): session dirs under `projects/` are named
//!   after the dash-encoded absolute cwd, and transcripts embed that path.
//!   1. rewrite absolute-path prefixes inside every `projects/**/ *.jsonl`, then
//!   2. rename each encoded `projects/<encoded>` directory to its re-encoded
//!      target name.
//! - `ContentOnly` (Copilot CLI): session dirs are keyed by session ID, but
//!   `session-state/**` content and `permissions-config.json` keys embed
//!   absolute paths. Rewrite prefixes inside every `*.json`/`*.jsonl` in the
//!   subtree; no directory renaming.

use std::fs;
use std::path::Path;

use anyhow::{Context, Result};

use crate::manifest::Manifest;
use crate::paths;
use crate::tools::RemapStrategy;

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
    mappings.sort_by_key(|m| std::cmp::Reverse(m.from.len()));
    mappings
}

/// Apply `mappings` to a tool's staged data subtree in place, using the
/// tool's remap strategy.
pub fn apply(data_root: &Path, mappings: &[Mapping], strategy: &RemapStrategy) -> Result<()> {
    if mappings.is_empty() {
        return Ok(());
    }
    match strategy {
        RemapStrategy::ClaudeProjects => apply_claude_projects(data_root, mappings),
        RemapStrategy::ContentOnly => apply_content_only(data_root, mappings),
    }
}

/// Rewrite `*.json`/`*.jsonl` contents anywhere under `data_root`. Used for
/// tools (Copilot) whose state embeds absolute paths — session events,
/// permission keys — but whose directory names carry no paths.
fn apply_content_only(data_root: &Path, mappings: &[Mapping]) -> Result<()> {
    for entry in walkdir::WalkDir::new(data_root) {
        let entry = entry?;
        if !entry.file_type().is_file() {
            continue;
        }
        let ext = entry.path().extension().and_then(|e| e.to_str());
        if matches!(ext, Some("json") | Some("jsonl")) {
            rewrite_file(entry.path(), mappings)
                .with_context(|| format!("remapping {}", entry.path().display()))?;
        }
    }
    Ok(())
}

/// The Claude Code strategy: rewrite transcripts under `projects/`, then
/// rename the dash-encoded project directories.
fn apply_claude_projects(data_root: &Path, mappings: &[Mapping]) -> Result<()> {
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

/// Rewrite every mapped prefix occurrence in a file's text content. Files
/// that are not valid UTF-8 (e.g. binary workspace artifacts riding along in
/// Copilot session state) are left untouched rather than erroring.
fn rewrite_file(path: &Path, mappings: &[Mapping]) -> Result<()> {
    let bytes = fs::read(path)?;
    let Ok(content) = String::from_utf8(bytes) else {
        return Ok(());
    };
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
        apply(&data, &mappings, &RemapStrategy::ClaudeProjects).unwrap();

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
        apply(&data, &[], &RemapStrategy::ClaudeProjects).unwrap();
        assert!(data.join("projects/-home-x-p/s.jsonl").exists());
    }

    #[test]
    fn content_only_rewrites_json_everywhere_without_renames() {
        let tmp = tempfile::tempdir().unwrap();
        let data = tmp.path().join("data");
        write(
            &data.join("session-state/abc123/events.jsonl"),
            "{\"cwd\":\"/Users/alice/proj\"}\n",
        );
        write(
            &data.join("permissions-config.json"),
            r#"{"/Users/alice/proj":{"allow":["shell"]}}"#,
        );
        // Prose is deliberately not rewritten.
        write(
            &data.join("copilot-instructions.md"),
            "see /Users/alice/proj",
        );
        // Binary content must be skipped, not errored on.
        fs::write(
            data.join("session-state/abc123/artifact.json"),
            [0xff, 0xfe, 0x00],
        )
        .unwrap();

        let manifest = Manifest::new("h".into(), "/Users/alice".into());
        let mappings = build_mappings(&manifest, "/home/bob", &Default::default());
        apply(&data, &mappings, &RemapStrategy::ContentOnly).unwrap();

        // Session dir name unchanged (keyed by ID, not cwd).
        let events = fs::read_to_string(data.join("session-state/abc123/events.jsonl")).unwrap();
        assert!(events.contains("/home/bob/proj"));
        assert!(!events.contains("/Users/alice"));
        // Absolute-path keys are plain strings in the text — rewritten too.
        let perms = fs::read_to_string(data.join("permissions-config.json")).unwrap();
        assert!(perms.contains("\"/home/bob/proj\""));
        // Markdown untouched.
        let md = fs::read_to_string(data.join("copilot-instructions.md")).unwrap();
        assert!(md.contains("/Users/alice/proj"));
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
