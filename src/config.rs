//! ccsync configuration. The config file (`~/.config/ccsync/config.toml`)
//! declares what to include/exclude from each tool's data directory, the git
//! remote used for sync, and any explicit path-remap pairs applied on restore.
//!
//! The top-level `include`/`exclude`/`include_sessions` fields are the
//! *Claude Code* policy (they predate multi-tool support and stay top-level so
//! existing configs keep working unchanged); the `[copilot]` table carries the
//! GitHub Copilot CLI policy.
//!
//! The defaults encode the portable-vs-sensitive split described in the design:
//! credentials and machine-local state are excluded; settings, memory, skills,
//! agents, and session transcripts are included.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

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
    /// Whether to bundle locally-configured MCP servers from `~/.claude.json`
    /// (user-scope and per-project `mcpServers`). Their `~/.claude.json` host
    /// file is otherwise never synced. Server definitions are still secret
    /// scanned, so a server `env` holding an API key aborts the snapshot unless
    /// `--allow-secrets` is passed.
    pub include_mcp_servers: bool,
    /// Git remote URL used by `push --git` / `pull --git`.
    pub remote: Option<String>,
    /// Explicit path remap pairs applied on restore, in addition to the
    /// automatic `source_home -> local_home` mapping. Keys are source prefixes,
    /// values are target prefixes.
    pub remap: BTreeMap<String, String>,
    /// Settings for the background service (`ccsync daemon` / `ccsync service`).
    pub service: ServiceConfig,
    /// Policy for the GitHub Copilot CLI source (`~/.copilot`).
    pub copilot: CopilotConfig,
}

/// Include/exclude policy for the GitHub Copilot CLI directory. Materialized
/// from the `[copilot]` table in `config.toml`; absent keys fall back to these
/// defaults, so configs written before the table existed keep loading.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct CopilotConfig {
    /// Whether to capture `~/.copilot` at all. A missing `~/.copilot` is
    /// always skipped silently, so leaving this on is harmless for users who
    /// don't run Copilot.
    pub enabled: bool,
    /// Relative paths under `~/.copilot` to include.
    pub include: Vec<String>,
    /// Relative paths (or path prefixes) under `~/.copilot` to always exclude.
    pub exclude: Vec<String>,
    /// Whether to capture session history (`session-state/`,
    /// `command-history-state/`).
    pub include_sessions: bool,
}

impl Default for CopilotConfig {
    fn default() -> Self {
        CopilotConfig {
            enabled: true,
            include: vec![
                "settings.json".into(),
                "mcp-config.json".into(),
                "copilot-instructions.md".into(),
                "instructions".into(),
                "agents".into(),
                "skills".into(),
                "extensions".into(),
                "hooks".into(),
                "lsp-config.json".into(),
                "permissions-config.json".into(),
                // Session history. Gated additionally by `include_sessions`.
                "session-state".into(),
                "command-history-state".into(),
            ],
            exclude: vec![
                // Sensitive: never sync. These duplicate the hard block in
                // `tools::spec` so default runs skip them silently instead of
                // aborting on the guard.
                "config.json".into(),
                "mcp-oauth-config".into(),
                "mcp-secrets".into(),
                // Machine-local / cache / runtime state.
                "logs".into(),
                "ide".into(),
                "installed-plugins".into(),
                "plugin-data".into(),
                // Binary SQLite checkpoint index; Copilot rebuilds it with
                // `/chronicle reindex`, and merging it is hopeless anyway.
                "session-store.db".into(),
            ],
            include_sessions: true,
        }
    }
}

/// Where the background service publishes each automatic backup.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ServiceDestination {
    /// Push to the configured git `remote` (same path as `ccsync push`).
    Git,
    /// Write a timestamped encrypted archive into `service.backup_dir`.
    Archive,
}

/// Configuration for the background backup service. Materialized from the
/// `[service]` table in `config.toml`; absent keys fall back to these defaults.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ServiceConfig {
    /// Master switch. When false, `ccsync daemon` exits immediately so that
    /// simply upgrading and re-saving the config never starts pushing.
    pub enabled: bool,
    /// Minutes to wait between automatic snapshot+publish ticks.
    pub interval_minutes: u64,
    /// Whether each tick pushes to the git remote or writes an encrypted
    /// archive.
    pub destination: ServiceDestination,
    /// Directory for timestamped archives when `destination = "archive"`.
    /// `None` means ccsync's managed backups dir (`~/.config/ccsync/backups`).
    pub backup_dir: Option<PathBuf>,
    /// Pass `--allow-secrets` to the unattended snapshot. Default false so the
    /// daemon fails closed if a config file looks like it contains a secret.
    pub allow_secrets: bool,
}

impl Default for ServiceConfig {
    fn default() -> Self {
        ServiceConfig {
            enabled: false,
            interval_minutes: 60,
            destination: ServiceDestination::Git,
            backup_dir: None,
            allow_secrets: false,
        }
    }
}

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
            include_mcp_servers: true,
            remote: None,
            remap: BTreeMap::new(),
            service: ServiceConfig::default(),
            copilot: CopilotConfig::default(),
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
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_exclude_credentials_and_caches() {
        let c = Config::default();
        let excluded = |rel: &str| crate::tools::is_excluded_by(&c.exclude, rel);
        assert!(excluded(".credentials.json"));
        assert!(excluded("shell-snapshots/snapshot-1.sh"));
        assert!(excluded("session-env/abc/x"));
        assert!(!excluded("settings.json"));
        assert!(!excluded("projects/-home-user-x/sess.jsonl"));
    }

    #[test]
    fn copilot_defaults_are_safe_and_on() {
        let c = CopilotConfig::default();
        assert!(c.enabled);
        assert!(c.include_sessions);
        let excluded = |rel: &str| crate::tools::is_excluded_by(&c.exclude, rel);
        assert!(excluded("config.json"));
        assert!(excluded("mcp-oauth-config/token.json"));
        assert!(excluded("mcp-secrets/index.json"));
        assert!(excluded("logs/process-1-2.log"));
        assert!(excluded("session-store.db"));
        assert!(!excluded("settings.json"));
        assert!(!excluded("session-state/abc/events.jsonl"));
    }

    #[test]
    fn config_without_copilot_table_loads_defaults() {
        // A config file written before `[copilot]` existed must still load,
        // picking up the current copilot defaults.
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("config.toml");
        std::fs::write(&path, "include = [\"settings.json\"]\n").unwrap();
        let loaded = Config::load(&path).unwrap();
        assert!(loaded.copilot.enabled);
        assert!(!loaded.copilot.include.is_empty());
    }

    #[test]
    fn partial_copilot_table_keeps_default_lists() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("config.toml");
        std::fs::write(&path, "[copilot]\nenabled = false\n").unwrap();
        let loaded = Config::load(&path).unwrap();
        assert!(!loaded.copilot.enabled);
        // Unspecified keys fall back to the defaults, not to empty.
        assert_eq!(loaded.copilot.include, CopilotConfig::default().include);
        assert!(loaded.copilot.include_sessions);
    }

    #[test]
    fn mcp_bundling_on_by_default_and_roundtrips() {
        assert!(Config::default().include_mcp_servers);

        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("config.toml");
        let c = Config {
            include_mcp_servers: false,
            ..Config::default()
        };
        c.save(&path).unwrap();
        assert!(!Config::load(&path).unwrap().include_mcp_servers);

        // A config written before the key existed loads with the default (on).
        std::fs::write(&path, "include = [\"settings.json\"]\n").unwrap();
        assert!(Config::load(&path).unwrap().include_mcp_servers);
    }

    #[test]
    fn config_roundtrips_toml() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("config.toml");
        let mut c = Config {
            remote: Some("git@example.com:me/ccsync-data.git".into()),
            ..Config::default()
        };
        c.remap.insert("/Users/alice".into(), "/home/alice".into());
        c.save(&path).unwrap();
        let loaded = Config::load(&path).unwrap();
        assert_eq!(
            loaded.remote.as_deref(),
            Some("git@example.com:me/ccsync-data.git")
        );
        assert_eq!(
            loaded.remap.get("/Users/alice").map(String::as_str),
            Some("/home/alice")
        );
    }

    #[test]
    fn service_defaults_are_conservative() {
        let s = ServiceConfig::default();
        assert!(!s.enabled);
        assert_eq!(s.interval_minutes, 60);
        assert_eq!(s.destination, ServiceDestination::Git);
        assert!(s.backup_dir.is_none());
        assert!(!s.allow_secrets);
    }

    #[test]
    fn config_with_service_roundtrips_toml() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("config.toml");
        let mut c = Config::default();
        c.service.enabled = true;
        c.service.interval_minutes = 15;
        c.service.destination = ServiceDestination::Archive;
        c.service.backup_dir = Some(PathBuf::from("/mnt/backups"));
        c.service.allow_secrets = true;
        c.save(&path).unwrap();
        let loaded = Config::load(&path).unwrap();
        assert!(loaded.service.enabled);
        assert_eq!(loaded.service.interval_minutes, 15);
        assert_eq!(loaded.service.destination, ServiceDestination::Archive);
        assert_eq!(
            loaded.service.backup_dir,
            Some(PathBuf::from("/mnt/backups"))
        );
        assert!(loaded.service.allow_secrets);
    }

    #[test]
    fn config_without_service_table_loads_defaults() {
        // A config file written before `[service]` existed must still load.
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("config.toml");
        std::fs::write(&path, "include = [\"settings.json\"]\n").unwrap();
        let loaded = Config::load(&path).unwrap();
        assert!(!loaded.service.enabled);
        assert_eq!(loaded.service.destination, ServiceDestination::Git);
    }
}
