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

use crate::manifest::{Manifest, ProjectRoot};
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
///
/// `project_roots` (from the manifest) is the authoritative encoded → decoded
/// table recorded on the source machine, where the real paths were known.
/// Falling back to `paths::decode_path` is lossy for paths containing dashes.
pub fn apply(data_root: &Path, mappings: &[Mapping], project_roots: &[ProjectRoot]) -> Result<()> {
    if mappings.is_empty() {
        return Ok(());
    }
    let projects = data_root.join("projects");
    if !projects.is_dir() {
        return Ok(());
    }

    // Transcripts also reference project dirs by their encoded names, so each
    // mapping is applied in raw form and in dash-encoded form.
    let encoded_mappings: Vec<Mapping> = mappings
        .iter()
        .map(|m| Mapping {
            from: paths::encode_path(Path::new(&m.from)),
            to: paths::encode_path(Path::new(&m.to)),
        })
        .collect();

    // 1. Rewrite transcript contents.
    for entry in walkdir::WalkDir::new(&projects) {
        let entry = entry?;
        if entry.file_type().is_file()
            && entry.path().extension().and_then(|e| e.to_str()) == Some("jsonl")
        {
            rewrite_file(entry.path(), mappings, &encoded_mappings)
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
        let naive = paths::decode_path(&encoded).to_string_lossy().to_string();
        let decoded = project_roots
            .iter()
            .find(|r| r.encoded == encoded)
            .map(|r| r.decoded_path.clone())
            .unwrap_or_else(|| naive.clone());
        let new_encoded = match remap_str(&decoded, mappings) {
            Some(new_decoded) => Some(paths::encode_path(Path::new(&new_decoded))),
            // An unresolved root (the snapshot fell back to the lossy decode)
            // can't match a mapping whose path has dashes, e.g. a home of
            // `/home/jean-luc`. Match its encoded form instead, the same
            // way the transcript contents were rewritten.
            None if decoded == naive => remap_encoded(&encoded, &encoded_mappings),
            None => None,
        };
        if let Some(new_encoded) = new_encoded {
            if new_encoded != encoded {
                renames.push((projects.join(&encoded), projects.join(&new_encoded)));
            }
        }
    }
    // Move every renamed dir aside first so a chain (`a` -> `b` while `b` ->
    // `c`) can't merge into a target that is itself about to move: the
    // result must not depend on `read_dir` order.
    let mut parked = Vec::with_capacity(renames.len());
    for (i, (from, to)) in renames.into_iter().enumerate() {
        let tmp = projects.join(format!(".ccsync-remap-{i}"));
        fs::rename(&from, &tmp)
            .with_context(|| format!("renaming {} -> {}", from.display(), tmp.display()))?;
        parked.push((tmp, to));
    }
    for (from, to) in parked {
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

/// Apply the first matching dash-encoded prefix mapping to an encoded dir
/// name.
fn remap_encoded(encoded: &str, encoded_mappings: &[Mapping]) -> Option<String> {
    encoded_mappings.iter().find_map(|m| {
        let rest = encoded.strip_prefix(&m.from)?;
        (rest.is_empty() || rest.starts_with('-')).then(|| format!("{}{rest}", m.to))
    })
}

/// Rewrite every mapped prefix occurrence in a file's text content. Matches
/// must end at a path boundary so remapping `/Users/alice` leaves the sibling
/// `/Users/alice2` (and `-Users-alice2` in encoded form) untouched.
///
/// Each form is rewritten in a single pass where the first (longest) matching
/// mapping wins and replaced text is never rescanned — the same semantics as
/// [`remap_str`], which renames the session dirs, so a transcript's `cwd`
/// always agrees with the dir it lands in.
fn rewrite_file(path: &Path, raw: &[Mapping], encoded: &[Mapping]) -> Result<()> {
    let content = fs::read_to_string(path)?;
    let mut out = content.clone();
    if let Some(replaced) = replace_bounded(&out, raw, raw_boundary) {
        out = replaced;
    }
    if let Some(replaced) = replace_bounded(&out, encoded, encoded_boundary) {
        out = replaced;
    }
    if out != content {
        fs::write(path, out)?;
    }
    Ok(())
}

/// Characters that may legally follow a raw absolute-path prefix.
fn raw_boundary(c: char) -> bool {
    matches!(c, '/' | '\\' | '"' | '\'') || c.is_whitespace()
}

/// Characters that may legally follow a dash-encoded path prefix (dash is the
/// separator in that form).
fn encoded_boundary(c: char) -> bool {
    matches!(c, '-' | '"' | '\'') || c.is_whitespace()
}

/// True for characters that belong to a path component; a match preceded by
/// one of these starts mid-component and must not be rewritten.
fn component_char(c: char) -> bool {
    c.is_alphanumeric() || matches!(c, '_' | '.')
}

/// Replace every occurrence of a mapping's `from` with its `to`, where the
/// match is not preceded by a component character and is followed by a
/// boundary character (or ends the text). At each position the first mapping
/// in `mappings` order that matches wins, and scanning resumes after the
/// replaced text. Returns `None` when nothing matched.
fn replace_bounded(
    text: &str,
    mappings: &[Mapping],
    boundary: impl Fn(char) -> bool,
) -> Option<String> {
    let mappings: Vec<&Mapping> = mappings.iter().filter(|m| !m.from.is_empty()).collect();
    if mappings.is_empty() {
        return None;
    }
    let mut out = String::new();
    let mut last = 0;
    let mut i = 0;
    let mut prev: Option<char> = None;
    while i < text.len() {
        let rest = &text[i..];
        let hit = if prev.is_some_and(component_char) {
            None
        } else {
            mappings.iter().find(|m| {
                rest.starts_with(m.from.as_str())
                    && rest[m.from.len()..].chars().next().is_none_or(&boundary)
            })
        };
        if let Some(m) = hit {
            out.push_str(&text[last..i]);
            out.push_str(&m.to);
            i += m.from.len();
            last = i;
            prev = m.from.chars().next_back();
        } else {
            let c = rest.chars().next().expect("i is inside text");
            i += c.len_utf8();
            prev = Some(c);
        }
    }
    if last == 0 {
        return None;
    }
    out.push_str(&text[last..]);
    Some(out)
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
        apply(&data, &mappings, &[]).unwrap();

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
        apply(&data, &[], &[]).unwrap();
        assert!(data.join("projects/-home-x-p/s.jsonl").exists());
    }

    #[test]
    fn rewrite_respects_path_boundaries() {
        let tmp = tempfile::tempdir().unwrap();
        let data = tmp.path().join("data");
        let sess = data.join("projects/-Users-alice-proj/s.jsonl");
        write(
            &sess,
            concat!(
                "{\"cwd\":\"/Users/alice/proj\",",
                "\"other\":\"/Users/alice2/proj\",",
                "\"enc\":\"-Users-alice-proj\",",
                "\"enc2\":\"-Users-alice2-proj\",",
                "\"home\":\"/Users/alice\"}\n",
            ),
        );

        let manifest = Manifest::new("h".into(), "/Users/alice".into());
        let mappings = build_mappings(&manifest, "/home/bob", &Default::default());
        apply(&data, &mappings, &[]).unwrap();

        let content = fs::read_to_string(data.join("projects/-home-bob-proj/s.jsonl")).unwrap();
        // The sibling user `/Users/alice2` (and its encoded form) is untouched.
        assert!(content.contains("\"other\":\"/Users/alice2/proj\""));
        assert!(content.contains("\"enc2\":\"-Users-alice2-proj\""));
        // The real home refs are rewritten in both forms, including a bare
        // home path terminated by a quote.
        assert!(content.contains("\"cwd\":\"/home/bob/proj\""));
        assert!(content.contains("\"enc\":\"-home-bob-proj\""));
        assert!(content.contains("\"home\":\"/home/bob\""));
    }

    #[test]
    fn manifest_roots_drive_dashed_dir_renames() {
        let tmp = tempfile::tempdir().unwrap();
        let data = tmp.path().join("data");
        // A project whose real path contains a dash: naive decode would read
        // this dir as /Users/alice/my/proj and an explicit mapping for the
        // real path would never match.
        write(
            &data.join("projects/-Users-alice-my-proj/s.jsonl"),
            "{\"cwd\":\"/Users/alice/my-proj\"}\n",
        );

        let manifest = Manifest::new("h".into(), "/Users/alice".into());
        let mappings = build_mappings(&manifest, "/home/bob", &Default::default());
        let roots = [ProjectRoot {
            encoded: "-Users-alice-my-proj".into(),
            decoded_path: "/Users/alice/my-proj".into(),
        }];
        apply(&data, &mappings, &roots).unwrap();

        // Renamed using the authoritative decoded path, so dir name and the
        // rewritten cwd stay in sync.
        let new_dir = data.join("projects/-home-bob-my-proj");
        assert!(new_dir.exists(), "expected dash-aware rename");
        let content = fs::read_to_string(new_dir.join("s.jsonl")).unwrap();
        assert!(content.contains("\"cwd\":\"/home/bob/my-proj\""));
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

    fn mapping(from: &str, to: &str) -> Mapping {
        Mapping {
            from: from.into(),
            to: to.into(),
        }
    }

    #[test]
    fn content_and_dir_agree_when_mappings_overlap() {
        // Explicit `/Users/alice/code/work -> /Users/alice/work` plus the
        // automatic home mapping: first match wins everywhere, so the rewritten
        // cwd is not fed through the home mapping a second time.
        let tmp = tempfile::tempdir().unwrap();
        let data = tmp.path().join("data");
        write(
            &data.join("projects/-Users-alice-code-work/s.jsonl"),
            "{\"cwd\":\"/Users/alice/code/work\"}\n",
        );
        let mut explicit = std::collections::BTreeMap::new();
        explicit.insert("/Users/alice/code/work".into(), "/Users/alice/work".into());
        let manifest = Manifest::new("h".into(), "/Users/alice".into());
        let mappings = build_mappings(&manifest, "/home/alice", &explicit);
        apply(&data, &mappings, &[]).unwrap();

        let dir = data.join("projects/-Users-alice-work");
        assert!(dir.is_dir(), "dir should follow the explicit mapping");
        let content = fs::read_to_string(dir.join("s.jsonl")).unwrap();
        assert!(
            content.contains("\"cwd\":\"/Users/alice/work\""),
            "{content}"
        );
    }

    #[test]
    fn chained_renames_do_not_depend_on_dir_order() {
        let tmp = tempfile::tempdir().unwrap();
        let data = tmp.path().join("data");
        write(&data.join("projects/-work-a/s.jsonl"), "{}\n");
        write(&data.join("projects/-src-a/t.jsonl"), "{}\n");
        let mappings = [mapping("/work", "/src"), mapping("/src", "/archive/src")];
        apply(&data, &mappings, &[]).unwrap();

        assert!(data.join("projects/-src-a/s.jsonl").exists());
        assert!(data.join("projects/-archive-src-a/t.jsonl").exists());
        assert!(!data.join("projects/-archive-src-a/s.jsonl").exists());
        assert!(!data.join("projects/-work-a").exists());
    }

    #[test]
    fn unresolved_roots_under_a_dashed_home_are_renamed() {
        let tmp = tempfile::tempdir().unwrap();
        let data = tmp.path().join("data");
        write(
            &data.join("projects/-home-jean-luc-old/s.jsonl"),
            "{\"cwd\":\"/home/jean-luc/old\"}\n",
        );
        // What snapshot records when it can't resolve the root: the naive decode.
        let roots = [ProjectRoot {
            encoded: "-home-jean-luc-old".into(),
            decoded_path: "/home/jean/luc/old".into(),
        }];
        apply(&data, &[mapping("/home/jean-luc", "/home/bob")], &roots).unwrap();

        let dir = data.join("projects/-home-bob-old");
        assert!(dir.is_dir());
        let content = fs::read_to_string(dir.join("s.jsonl")).unwrap();
        assert!(content.contains("\"cwd\":\"/home/bob/old\""));
    }

    #[test]
    fn resolved_sibling_roots_are_not_renamed_by_the_encoded_fallback() {
        let tmp = tempfile::tempdir().unwrap();
        let data = tmp.path().join("data");
        write(&data.join("projects/-Users-alice-2-proj/s.jsonl"), "{}\n");
        let roots = [ProjectRoot {
            encoded: "-Users-alice-2-proj".into(),
            decoded_path: "/Users/alice-2/proj".into(),
        }];
        apply(&data, &[mapping("/Users/alice", "/home/bob")], &roots).unwrap();
        assert!(data.join("projects/-Users-alice-2-proj").is_dir());
    }
}
