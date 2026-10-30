//! Locating the Claude Code data directory and translating between absolute
//! working-directory paths and the encoded directory names Claude Code uses
//! under `~/.claude/projects/`.
//!
//! Claude Code names each project's session directory after the absolute path
//! of the working directory, replacing every path separator with a dash. For
//! example `/home/user/ccsync` becomes `-home-user-ccsync`. This module owns
//! that encoding so the snapshot and remap logic can rely on a single
//! implementation.
//!
//! ccsync's own files live under a per-platform config directory, written
//! below as `<config>/ccsync/`. `<config>` is `dirs::config_dir()`, which is
//! `~/Library/Application Support` on macOS and `~/.config` on Linux (or
//! `$XDG_CONFIG_HOME` when set) — *not* always `~/.config`.

use std::fs;
use std::path::{Path, PathBuf};

use crate::error::CcError;

/// Locate the Claude Code config directory.
///
/// Honors `CLAUDE_CONFIG_DIR` (Linux/Windows override) first, then falls back
/// to `~/.claude`.
pub fn claude_dir() -> Result<PathBuf, CcError> {
    if let Ok(dir) = std::env::var("CLAUDE_CONFIG_DIR") {
        if !dir.is_empty() {
            return Ok(PathBuf::from(dir));
        }
    }
    let home = dirs::home_dir().ok_or(CcError::ClaudeDirNotFound)?;
    Ok(home.join(".claude"))
}

/// The user's home directory, used as the default remap source/target.
pub fn home_dir() -> Result<PathBuf, CcError> {
    dirs::home_dir().ok_or(CcError::ClaudeDirNotFound)
}

/// Locate Claude Code's `~/.claude.json`, the file that holds MCP server
/// definitions (among other machine-local state).
///
/// When `CLAUDE_CONFIG_DIR` is set, Claude Code keeps `.claude.json` inside that
/// directory; otherwise it lives at `~/.claude.json` (a sibling of `~/.claude`).
pub fn claude_json_file() -> Result<PathBuf, CcError> {
    if let Ok(dir) = std::env::var("CLAUDE_CONFIG_DIR") {
        if !dir.is_empty() {
            return Ok(PathBuf::from(dir).join(".claude.json"));
        }
    }
    Ok(home_dir()?.join(".claude.json"))
}

/// ccsync's own config file location: `<config>/ccsync/config.toml`.
pub fn config_file() -> Result<PathBuf, CcError> {
    let base = dirs::config_dir().ok_or(CcError::ClaudeDirNotFound)?;
    Ok(base.join("ccsync").join("config.toml"))
}

/// ccsync's staging directory where a snapshot is materialized before it is
/// pushed or after it is pulled: `<config>/ccsync/staging`.
pub fn staging_dir() -> Result<PathBuf, CcError> {
    let base = dirs::config_dir().ok_or(CcError::ClaudeDirNotFound)?;
    Ok(base.join("ccsync").join("staging"))
}

/// The background daemon's private staging dir,
/// `<config>/ccsync/daemon-staging`, kept apart from [`staging_dir`] so a
/// scheduled backup never clobbers a snapshot pulled for `restore`.
pub fn daemon_staging_dir() -> Result<PathBuf, CcError> {
    let base = dirs::config_dir().ok_or(CcError::ClaudeDirNotFound)?;
    Ok(base.join("ccsync").join("daemon-staging"))
}

/// Marker file inside `staging` recording that it holds a snapshot brought
/// in by `pull`/`import` rather than one taken on this machine. `push`
/// refuses such staging (it would publish another machine's or an old
/// snapshot as this machine's), and the next snapshot build clears it.
pub fn pulled_marker(staging: &Path) -> PathBuf {
    staging.join(".ccsync-pulled")
}

/// ccsync's managed local-backups directory where the TUI writes timestamped
/// encrypted archives so they can be listed later: `<config>/ccsync/backups`.
pub fn backups_dir() -> Result<PathBuf, CcError> {
    let base = dirs::config_dir().ok_or(CcError::ClaudeDirNotFound)?;
    Ok(base.join("ccsync").join("backups"))
}

/// ccsync's profile store: `<config>/ccsync/profiles/<name>/{profile.toml,
/// data/,mcp-servers.json}` plus `active.json` marking the active profile.
pub fn profiles_dir() -> Result<PathBuf, CcError> {
    let base = dirs::config_dir().ok_or(CcError::ClaudeDirNotFound)?;
    Ok(base.join("ccsync").join("profiles"))
}

/// Checkouts of read-only shared layers: `<config>/ccsync/layers/<name>/`.
pub fn layers_dir() -> Result<PathBuf, CcError> {
    let base = dirs::config_dir().ok_or(CcError::ClaudeDirNotFound)?;
    Ok(base.join("ccsync").join("layers"))
}

/// Environment file the installed service sources for secrets the service
/// manager does not inherit (notably `CCSYNC_PASSPHRASE`):
/// `<config>/ccsync/service.env`. Created by the user, never by ccsync.
pub fn service_env_file() -> Result<PathBuf, CcError> {
    let base = dirs::config_dir().ok_or(CcError::ClaudeDirNotFound)?;
    Ok(base.join("ccsync").join("service.env"))
}

/// PID file for a detached daemon started with `ccsync service start`:
/// `<config>/ccsync/daemon.pid`.
pub fn daemon_pidfile() -> Result<PathBuf, CcError> {
    let base = dirs::config_dir().ok_or(CcError::ClaudeDirNotFound)?;
    Ok(base.join("ccsync").join("daemon.pid"))
}

/// Log file a detached daemon redirects its output to (tail it like a screen
/// session): `<config>/ccsync/daemon.log`.
pub fn daemon_logfile() -> Result<PathBuf, CcError> {
    let base = dirs::config_dir().ok_or(CcError::ClaudeDirNotFound)?;
    Ok(base.join("ccsync").join("daemon.log"))
}

/// Longest project-directory name Claude Code writes before truncating it
/// and appending a hash of the full path (see [`encode_path`]).
pub const MAX_ENCODED_LEN: usize = 200;

/// The character mapping of Claude Code's project-directory encoding, without
/// the length cap: every character that is not an ASCII letter or digit
/// becomes `-`. Mirrors JS `s.replace(/[^a-zA-Z0-9]/g, "-")`, which works on
/// UTF-16 code units, so a character outside the BMP becomes *two* dashes.
///
/// Use this for path *prefixes* (remap mappings, home siblings), where the
/// truncation of a whole name must not apply; use [`encode_path`] for the
/// name of a project directory itself.
pub fn encode_prefix(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c);
        } else {
            for _ in 0..c.len_utf16() {
                out.push('-');
            }
        }
    }
    out
}

/// Encode an absolute path into the project-directory name Claude Code uses
/// under `projects/`: `/home/user/my_app` -> `-home-user-my-app`. Names longer
/// than [`MAX_ENCODED_LEN`] are cut there and suffixed with `-<hash>`, where
/// the hash is computed over the *original* path, exactly as Claude Code does.
///
/// The encoding is many-to-one (`/a/b-c`, `/a/b.c` and `/a/b/c` all encode to
/// `-a-b-c`), so it can never be reliably inverted: decode through
/// `manifest.project_roots` instead.
pub fn encode_path(path: &Path) -> String {
    let s = path.to_string_lossy();
    let encoded = encode_prefix(&s);
    if encoded.len() <= MAX_ENCODED_LEN {
        return encoded;
    }
    // `encoded` is pure ASCII, so byte and UTF-16 lengths agree.
    format!(
        "{}-{}",
        &encoded[..MAX_ENCODED_LEN],
        to_base36(js_string_hash(&s).unsigned_abs())
    )
}

/// The 32-bit string hash Claude Code suffixes truncated names with:
/// `h = (h << 5) - h + charCode | 0` over UTF-16 code units.
fn js_string_hash(s: &str) -> i32 {
    s.encode_utf16().fold(0i32, |h, unit| {
        h.wrapping_shl(5).wrapping_sub(h).wrapping_add(unit as i32)
    })
}

/// Lowercase base-36 rendering, matching JS `Number.prototype.toString(36)`.
fn to_base36(mut n: u32) -> String {
    const DIGITS: &[u8] = b"0123456789abcdefghijklmnopqrstuvwxyz";
    if n == 0 {
        return "0".into();
    }
    let mut out = Vec::new();
    while n > 0 {
        out.push(DIGITS[(n % 36) as usize]);
        n /= 36;
    }
    out.reverse();
    String::from_utf8(out).expect("base36 digits are ASCII")
}

/// Naive decode of a project directory name: every dash becomes a separator.
/// `-home-user-ccsync` -> `/home/user/ccsync`.
///
/// Wrong for any path containing a dash or any other non-alphanumeric
/// character (all of which encode to `-`), and for truncated names. It is only
/// a last resort when neither `~/.claude.json` nor the filesystem resolves a
/// name; prefer `manifest.project_roots` whenever a manifest is available.
pub fn decode_path(encoded: &str) -> PathBuf {
    PathBuf::from(encoded.replace('-', "/"))
}

/// Resolve an encoded project-directory name against the live filesystem by
/// walking down from `/` and keeping every directory whose encoded path is
/// still a prefix of `encoded`. That disambiguates dashes that encode a
/// separator from dashes that stand for a literal `-`, `.`, `_`, space, etc.
/// Shorter names are tried first at each level, so when both `/a/b` and
/// `/a-b` exist the all-separators reading wins.
///
/// A truncated name (longer than [`MAX_ENCODED_LEN`]) resolves only when the
/// cut falls inside its last component, since the walk does not descend past
/// the encoded prefix it can check; `~/.claude.json` covers the rest.
/// Returns `None` when nothing matches (e.g. the project was deleted since
/// the session was recorded).
pub fn resolve_encoded_on_disk(encoded: &str) -> Option<PathBuf> {
    if !encoded.starts_with('-') {
        return None;
    }
    let prefix = if encoded.len() > MAX_ENCODED_LEN {
        encoded.get(..MAX_ENCODED_LEN)?
    } else {
        encoded
    };

    fn dfs(base: &Path, base_enc: &str, encoded: &str, prefix: &str) -> Option<PathBuf> {
        let mut children: Vec<(String, PathBuf)> = fs::read_dir(base)
            .ok()?
            .filter_map(|e| e.ok())
            .filter_map(|e| {
                let name = e.file_name().to_str()?.to_string();
                let enc = format!("{base_enc}-{}", encode_prefix(&name));
                // Keep only children still consistent with the target.
                let n = enc.len().min(prefix.len());
                (enc[..n] == prefix[..n]).then(|| (enc, e.path()))
            })
            .collect();
        children.sort_by(|a, b| a.0.len().cmp(&b.0.len()).then_with(|| a.1.cmp(&b.1)));
        for (enc, path) in children {
            if !path.is_dir() {
                continue;
            }
            if encode_path(&path) == encoded {
                return Some(path);
            }
            if enc.len() < prefix.len() {
                if let Some(found) = dfs(&path, &enc, encoded, prefix) {
                    return Some(found);
                }
            }
        }
        None
    }
    dfs(Path::new("/"), "", encoded, prefix)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_roundtrip_unix() {
        let p = Path::new("/home/user/ccsync");
        assert_eq!(encode_path(p), "-home-user-ccsync");
        assert_eq!(
            decode_path("-home-user-ccsync"),
            PathBuf::from("/home/user/ccsync")
        );
    }

    #[test]
    fn encode_handles_nested() {
        let p = Path::new("/Users/alice/code/proj");
        assert_eq!(encode_path(p), "-Users-alice-code-proj");
    }

    #[test]
    fn decode_is_inverse_of_encode_for_dashless_paths() {
        let p = Path::new("/var/tmp/work");
        let encoded = encode_path(p);
        assert_eq!(decode_path(&encoded), p);
    }

    #[test]
    fn resolves_dashed_dirs_against_the_filesystem() {
        // A real directory whose name contains a dash: the naive decode of its
        // encoded form is wrong, but the disk walk recovers the true path.
        let tmp = tempfile::tempdir().unwrap();
        let real = tmp.path().join("my-proj").join("sub");
        std::fs::create_dir_all(&real).unwrap();

        let encoded = encode_path(&real);
        assert_ne!(decode_path(&encoded), real, "naive decode should be lossy");
        assert_eq!(resolve_encoded_on_disk(&encoded), Some(real));
    }

    #[test]
    fn encodes_every_non_alphanumeric_char_like_claude_code() {
        // Reference values from Claude Code's own encoder
        // (`s.replace(/[^a-zA-Z0-9]/g, "-")`): a BMP char is one dash, an
        // astral char (two UTF-16 units) is two.
        let p = Path::new("/Users/jean.luc/my_app v2/é/😀x");
        assert_eq!(encode_path(p), "-Users-jean-luc-my-app-v2-----x");
        assert_eq!(encode_prefix("/home/jean.luc"), "-home-jean-luc");
    }

    #[test]
    fn truncates_long_names_with_claude_codes_hash() {
        let base = format!("/home/user/{}", "very_long.directory-name/".repeat(9));
        // Positive and negative 32-bit hashes; suffixes computed by Claude
        // Code's function in node.
        for (last, suffix) in [("proj", "ddq2fp"), ("proj2", "bd88ar")] {
            let path = format!("{base}{last}");
            let enc = encode_path(Path::new(&path));
            assert_eq!(enc.len(), MAX_ENCODED_LEN + 1 + suffix.len(), "{enc}");
            assert_eq!(
                &enc[..MAX_ENCODED_LEN],
                &encode_prefix(&path)[..MAX_ENCODED_LEN]
            );
            assert!(enc.ends_with(&format!("-{suffix}")), "{enc}");
        }
        // Exactly at the cap: no suffix.
        let at_cap = format!("/{}", "a".repeat(MAX_ENCODED_LEN - 1));
        assert_eq!(encode_path(Path::new(&at_cap)).len(), MAX_ENCODED_LEN);
    }

    #[test]
    fn resolves_dirs_with_dots_underscores_and_spaces() {
        let tmp = tempfile::tempdir().unwrap();
        let real = tmp.path().join("jean.luc").join("my_app v2").join("x-y");
        std::fs::create_dir_all(&real).unwrap();
        // Decoys whose encodings share prefixes with the real path.
        std::fs::create_dir_all(tmp.path().join("jean")).unwrap();
        std::fs::create_dir_all(tmp.path().join("jean.luc").join("my")).unwrap();

        let encoded = encode_path(&real);
        assert_eq!(resolve_encoded_on_disk(&encoded), Some(real));
    }

    #[test]
    fn resolves_a_truncated_name_cut_inside_its_last_component() {
        let tmp = tempfile::tempdir().unwrap();
        let real = tmp
            .path()
            .join(format!("proj_{}", "x".repeat(MAX_ENCODED_LEN)));
        std::fs::create_dir_all(&real).unwrap();
        let encoded = encode_path(&real);
        assert!(encoded.len() > MAX_ENCODED_LEN);
        assert_eq!(resolve_encoded_on_disk(&encoded), Some(real));
    }

    #[test]
    fn resolve_returns_none_for_missing_dirs() {
        assert_eq!(
            resolve_encoded_on_disk("-definitely-not-a-real-ccsync-path-xyz"),
            None
        );
    }
}
