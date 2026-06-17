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

/// ccsync's managed local-backups directory where the TUI writes timestamped
/// encrypted archives so they can be listed later: `<config>/ccsync/backups`.
pub fn backups_dir() -> Result<PathBuf, CcError> {
    let base = dirs::config_dir().ok_or(CcError::ClaudeDirNotFound)?;
    Ok(base.join("ccsync").join("backups"))
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

/// Encode an absolute path into the dash-separated form Claude Code uses for
/// project directory names. `/home/user/ccsync` -> `-home-user-ccsync`.
pub fn encode_path(path: &Path) -> String {
    let s = path.to_string_lossy();
    // Replace both Unix and Windows separators with a dash so the encoding is
    // stable regardless of the platform that produced the path.
    s.chars()
        .map(|c| if c == '/' || c == '\\' { '-' } else { c })
        .collect()
}

/// Decode a Claude Code project directory name back into an absolute path.
/// `-home-user-ccsync` -> `/home/user/ccsync`.
///
/// This is inherently lossy because the original path could itself contain
/// dashes, but Claude Code's own decoding makes the same assumption, so we
/// mirror it: every dash becomes a separator.
pub fn decode_path(encoded: &str) -> PathBuf {
    PathBuf::from(encoded.replace('-', "/"))
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
}
