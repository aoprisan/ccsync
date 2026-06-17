//! ccsync configuration. The config file (`~/.config/ccsync/config.toml`)
//! declares what to include/exclude from the Claude Code directory, the git
//! remote used for sync, and any explicit path-remap pairs applied on restore.
//!
//! The defaults encode the portable-vs-sensitive split described in the design:
//! credentials and machine-local state are excluded; settings, memory, skills,
//! agents, and session transcripts are included.

use std::collections::BTreeMap;
use std::path::Path;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Relative paths under `~/.claude` to include. A trailing entry that names
    /// a directory includes the whole tree (minus `exclude`).
    pub include: Vec<String>,
    /// Relative paths (or path prefixes) under `~/.claude` to always exclude,
    /// even if matched by `include`.
    pub exclude: Vec<String>,
    /// Whether to capture session transcripts under `projects/`.
    pub include_sessions: bool,
    /// Git remote URL used by `push --git` / `pull --git`.
    pub remote: Option<String>,
    /// Explicit path remap pairs applied on restore, in addition to the
    /// automatic `source_home -> local_home` mapping. Keys are source prefixes,
    /// values are target prefixes.
    pub remap: BTreeMap<String, String>,
}

/// File names that are credentials and must never be captured, regardless of
/// configuration. Enforced in `snapshot`/`redact` as a hard block.
pub const CREDENTIAL_BLOCKLIST: &[&str] = &[".credentials.json"];

impl Default for Config {
    fn default() -> Self {
        Config {
            include: vec![
                "settings.json".into(),
                "CLAUDE.md".into(),
                "keybindings.json".into(),
                "rules".into(),
                "skills".into(),
                "commands".into(),
                "agents".into(),
                "agent-memory".into(),
                "output-styles".into(),
                "workflows".into(),
                "themes".into(),
                // Session transcripts + per-repo auto memory. Gated additionally
                // by `include_sessions`.
                "projects".into(),
            ],
            exclude: vec![
                // Sensitive: never sync.
                ".credentials.json".into(),
                // Machine-local / cache / runtime state.
                "shell-snapshots".into(),
                "session-env".into(),
                "backups".into(),
                "statsig".into(),
                ".last-cleanup".into(),
                "launcher-settings.json".into(),
                "policy-limits.json".into(),
                "remote-settings.json".into(),
            ],
            include_sessions: true,
            remote: None,
            remap: BTreeMap::new(),
        }
    }
}

impl Config {
    /// Load the config from `path`, falling back to defaults if it does not
    /// exist.
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        if !path.exists() {
            return Ok(Config::default());
        }
        let text = std::fs::read_to_string(path)?;
        let cfg: Config = toml::from_str(&text)?;
        Ok(cfg)
    }

    /// Serialize the config to TOML, creating parent directories as needed.
    pub fn save(&self, path: &Path) -> anyhow::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let text = toml::to_string_pretty(self)?;
        std::fs::write(path, text)?;
        Ok(())
    }

    /// True if `rel` (a path relative to `~/.claude`) is excluded by any
    /// configured exclude prefix.
    pub fn is_excluded(&self, rel: &str) -> bool {
        let rel = rel.replace('\\', "/");
        self.exclude.iter().any(|ex| {
            let ex = ex.trim_end_matches('/');
            rel == ex || rel.starts_with(&format!("{ex}/"))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_exclude_credentials_and_caches() {
        let c = Config::default();
        assert!(c.is_excluded(".credentials.json"));
        assert!(c.is_excluded("shell-snapshots/snapshot-1.sh"));
        assert!(c.is_excluded("session-env/abc/x"));
        assert!(!c.is_excluded("settings.json"));
        assert!(!c.is_excluded("projects/-home-user-x/sess.jsonl"));
    }

    #[test]
    fn config_roundtrips_toml() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("config.toml");
        let mut c = Config::default();
        c.remote = Some("git@example.com:me/ccsync-data.git".into());
        c.remap.insert("/Users/alice".into(), "/home/alice".into());
        c.save(&path).unwrap();
        let loaded = Config::load(&path).unwrap();
        assert_eq!(loaded.remote.as_deref(), Some("git@example.com:me/ccsync-data.git"));
        assert_eq!(loaded.remap.get("/Users/alice").map(String::as_str), Some("/home/alice"));
    }
}
