//! ccsync configuration. The config file (`~/.config/ccsync/config.toml`)
//! declares what to include/exclude from the Claude Code directory, the git
//! remote used for sync, and any explicit path-remap pairs applied on restore.
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
    /// How to handle secret-shaped strings found in session transcripts
    /// (`projects/**/*.jsonl`). Transcripts legitimately discuss secrets, so
    /// aborting like config files do would make snapshots unusable; the
    /// default rewrites each matched span to `[REDACTED:ccsync]` in the
    /// *staged copy only* — source files are never touched.
    pub transcript_secrets: TranscriptSecrets,
    /// Require confirmation before a restore installs new hook commands from
    /// an incoming settings.json. Hooks are arbitrary shell commands that will
    /// run in Claude Code sessions on this machine.
    pub confirm_hooks: bool,
    /// Git remote URL used by `push --git` / `pull --git`.
    pub remote: Option<String>,
    /// Stable identity for this machine's subtree in the sync repo
    /// (`machines/<id>/`). Defaults to the hostname and is persisted here on
    /// the first push so a later hostname change doesn't fork the history.
    pub machine_id: Option<String>,
    /// Explicit path remap pairs applied on restore, in addition to the
    /// automatic `source_home -> local_home` mapping. Keys are source prefixes,
    /// values are target prefixes.
    pub remap: BTreeMap<String, String>,
    /// Settings for the background service (`ccsync daemon` / `ccsync service`).
    pub service: ServiceConfig,
    /// Settings for named profiles (`ccsync profile ...`).
    pub profiles: ProfilesConfig,
    /// Per-machine additions, keyed by machine id and merged over the base
    /// config at load time on the matching machine (see
    /// [`Config::with_machine_overrides`]). Skipped when empty so a saved
    /// config stays hand-editable (no stray `machines = {}` blocking a later
    /// `[machines.<id>]` table).
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub machines: BTreeMap<String, MachineOverrides>,
    /// Read-only shared layers (e.g. a team's skills/commands repo), pulled
    /// with `ccsync layer pull` and applied with `ccsync layer apply`.
    /// Skipped when empty so `[[layers]]` can be appended by hand.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub layers: Vec<LayerConfig>,
    /// Policy for the GitHub Copilot CLI tree (`~/.copilot`), captured under
    /// the reserved `ccsync-copilot/` snapshot component.
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

impl CopilotConfig {
    /// True if `rel` (a path relative to `~/.copilot`) is excluded by any
    /// configured exclude prefix.
    pub fn is_excluded(&self, rel: &str) -> bool {
        is_excluded_by(&self.exclude, rel)
    }
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
                // `copilot::credential_block_match` so default runs skip them
                // silently instead of aborting on the guard (same pattern as
                // Claude's `.credentials.json` below).
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

/// One `[[layers]]` entry: a git repo whose declared top-level components are
/// copied into `~/.claude` on `layer apply`. Layers are read-only sources —
/// ccsync never pushes to them.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct LayerConfig {
    /// Local name (also the checkout directory under `<config>/ccsync/layers/`).
    pub name: String,
    /// Git URL of the shared repo.
    pub remote: String,
    /// Top-level components of the repo to apply (e.g. `["skills", "commands"]`).
    /// Nothing outside this list is ever copied.
    pub components: Vec<String>,
}

/// Extra include/exclude/remap entries that apply on one machine only, e.g.
/// `[machines.laptop]` with `exclude_extra = ["projects"]`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct MachineOverrides {
    /// Appended to `include`.
    pub include_extra: Vec<String>,
    /// Appended to `exclude` (exclusion wins over inclusion, as always).
    pub exclude_extra: Vec<String>,
    /// Merged into `[remap]` (machine entry wins on conflicts).
    pub remap: BTreeMap<String, String>,
}

/// Configuration for named profiles: which parts of `~/.claude` a profile
/// owns, and whether the profile store rides along inside snapshots.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ProfilesConfig {
    /// Top-level components of `~/.claude` a profile owns. Everything else
    /// (sessions, agent memory, keybindings, ...) is shared base state that
    /// survives a switch untouched.
    pub components: Vec<String>,
    /// Whether a profile also owns the user-scope `mcpServers` of
    /// `~/.claude.json` (per-project servers are tied to directories, not
    /// environments, and are never touched).
    pub include_user_mcp: bool,
    /// Bundle the profile store into snapshots so profiles sync across
    /// machines with the normal push/pull/export flow.
    pub sync: bool,
}

impl Default for ProfilesConfig {
    fn default() -> Self {
        ProfilesConfig {
            components: vec![
                "settings.json".into(),
                "CLAUDE.md".into(),
                "agents".into(),
                "skills".into(),
                "commands".into(),
                "output-styles".into(),
            ],
            include_user_mcp: true,
            sync: true,
        }
    }
}

/// Policy for secret-shaped strings inside session transcripts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TranscriptSecrets {
    /// Replace each match with `[REDACTED:ccsync]` in the staged copy.
    #[default]
    Redact,
    /// Abort the snapshot, exactly like a secret in a config file.
    Abort,
    /// Capture transcripts verbatim.
    Ignore,
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
                // Plugin configuration (the re-fetchable clones under it are
                // excluded below).
                "plugins".into(),
                // Session transcripts + per-repo auto memory. Gated additionally
                // by `include_sessions`.
                "projects".into(),
                // Per-session todo state; travels with the sessions.
                "todos".into(),
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
                "settings.local.json".into(),
                "ide".into(),
                // Plugin checkouts/caches are large and re-fetchable.
                "plugins/repos".into(),
                "plugins/cache".into(),
                "plugins/marketplaces".into(),
            ],
            include_sessions: true,
            include_mcp_servers: true,
            transcript_secrets: TranscriptSecrets::default(),
            confirm_hooks: true,
            remote: None,
            machine_id: None,
            remap: BTreeMap::new(),
            service: ServiceConfig::default(),
            profiles: ProfilesConfig::default(),
            machines: BTreeMap::new(),
            layers: Vec::new(),
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

    /// The effective machine identity used for this machine's `machines/<id>/`
    /// subtree in the sync repo: the configured `machine_id`, else the
    /// hostname, sanitized to a safe directory name.
    pub fn effective_machine_id(&self) -> String {
        let raw = self
            .machine_id
            .clone()
            .unwrap_or_else(crate::snapshot::hostname);
        let id: String = raw
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') {
                    c
                } else {
                    '-'
                }
            })
            .collect();
        let id = id.trim_matches('.').to_string();
        if id.is_empty() {
            "default".to_string()
        } else {
            id
        }
    }

    /// Fold this machine's `[machines.<id>]` overrides into the base config.
    /// Callers must NOT save the result back to disk — the overrides would be
    /// baked into the base sets; persist from a freshly-loaded copy instead.
    pub fn with_machine_overrides(mut self) -> Self {
        let id = self.effective_machine_id();
        if let Some(overrides) = self.machines.get(&id).cloned() {
            for inc in overrides.include_extra {
                if !self.include.contains(&inc) {
                    self.include.push(inc);
                }
            }
            for exc in overrides.exclude_extra {
                if !self.exclude.contains(&exc) {
                    self.exclude.push(exc);
                }
            }
            self.remap.extend(overrides.remap);
        }
        self
    }

    /// True if `rel` (a path relative to `~/.claude`) is excluded by any
    /// configured exclude prefix.
    pub fn is_excluded(&self, rel: &str) -> bool {
        is_excluded_by(&self.exclude, rel)
    }
}

/// Prefix-exclusion check shared by the Claude and Copilot policies.
fn is_excluded_by(exclude: &[String], rel: &str) -> bool {
    let rel = rel.replace('\\', "/");
    exclude.iter().any(|ex| {
        let ex = ex.trim_end_matches('/');
        rel == ex || rel.starts_with(&format!("{ex}/"))
    })
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
    fn defaults_classify_plugins_todos_and_local_state() {
        let c = Config::default();
        // Newly-classified entries: plugin config travels, its checkouts don't.
        assert!(c.include.iter().any(|i| i == "plugins"));
        assert!(c.include.iter().any(|i| i == "todos"));
        assert!(!c.is_excluded("plugins/config.json"));
        assert!(c.is_excluded("plugins/repos/org/repo/index.js"));
        assert!(c.is_excluded("plugins/cache/x"));
        // Machine-local by convention.
        assert!(c.is_excluded("settings.local.json"));
        assert!(c.is_excluded("ide/lock"));
    }

    #[test]
    fn secret_policy_defaults_and_back_compat() {
        let c = Config::default();
        assert_eq!(c.transcript_secrets, TranscriptSecrets::Redact);
        assert!(c.confirm_hooks);

        // A config written before these fields existed loads the defaults.
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("config.toml");
        std::fs::write(&path, "include = [\"settings.json\"]\n").unwrap();
        let loaded = Config::load(&path).unwrap();
        assert_eq!(loaded.transcript_secrets, TranscriptSecrets::Redact);
        assert!(loaded.confirm_hooks);

        // And the policy round-trips.
        let c = Config {
            transcript_secrets: TranscriptSecrets::Abort,
            confirm_hooks: false,
            ..Config::default()
        };
        c.save(&path).unwrap();
        let loaded = Config::load(&path).unwrap();
        assert_eq!(loaded.transcript_secrets, TranscriptSecrets::Abort);
        assert!(!loaded.confirm_hooks);
    }

    #[test]
    fn machine_overrides_fold_in_only_for_matching_id() {
        let mut c = Config {
            machine_id: Some("laptop".into()),
            ..Config::default()
        };
        c.machines.insert(
            "laptop".into(),
            MachineOverrides {
                include_extra: vec!["extra-dir".into()],
                exclude_extra: vec!["projects".into()],
                remap: [("/a".to_string(), "/b".to_string())].into(),
            },
        );
        c.machines.insert(
            "other".into(),
            MachineOverrides {
                include_extra: vec!["never-here".into()],
                ..Default::default()
            },
        );

        let effective = c.with_machine_overrides();
        assert!(effective.include.iter().any(|i| i == "extra-dir"));
        assert!(!effective.include.iter().any(|i| i == "never-here"));
        assert!(effective.is_excluded("projects/x/s.jsonl"));
        assert_eq!(effective.remap.get("/a").map(String::as_str), Some("/b"));

        // Back-compat: configs without [machines] load fine.
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("config.toml");
        std::fs::write(&path, "include = [\"settings.json\"]\n").unwrap();
        assert!(Config::load(&path).unwrap().machines.is_empty());
    }

    #[test]
    fn saved_config_accepts_appended_layer_and_machine_tables() {
        // A default save must not emit empty `machines`/`layers` keys, or a
        // user appending `[[layers]]` / `[machines.x]` by hand gets duplicate
        // key errors.
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("config.toml");
        Config::default().save(&path).unwrap();
        let mut text = std::fs::read_to_string(&path).unwrap();
        text.push_str(
            "\n[[layers]]\nname = \"team\"\nremote = \"git@x:y.git\"\ncomponents = [\"skills\"]\n\
             \n[machines.laptop]\nexclude_extra = [\"projects\"]\n",
        );
        std::fs::write(&path, text).unwrap();
        let loaded = Config::load(&path).unwrap();
        assert_eq!(loaded.layers.len(), 1);
        assert_eq!(loaded.layers[0].name, "team");
        assert!(loaded.machines.contains_key("laptop"));
    }

    #[test]
    fn effective_machine_id_sanitizes() {
        let c = Config {
            machine_id: Some("Al's MacBook Pro!".into()),
            ..Config::default()
        };
        assert_eq!(c.effective_machine_id(), "Al-s-MacBook-Pro-");
        let c = Config {
            machine_id: Some("...".into()),
            ..Config::default()
        };
        assert_eq!(c.effective_machine_id(), "default");
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

    #[test]
    fn copilot_defaults_are_safe_and_on() {
        let c = CopilotConfig::default();
        assert!(c.enabled);
        assert!(c.include_sessions);
        assert!(c.is_excluded("config.json"));
        assert!(c.is_excluded("mcp-oauth-config/token.json"));
        assert!(c.is_excluded("mcp-secrets/index.json"));
        assert!(c.is_excluded("logs/process-1-2.log"));
        assert!(c.is_excluded("session-store.db"));
        assert!(!c.is_excluded("settings.json"));
        assert!(!c.is_excluded("session-state/abc/events.jsonl"));
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
}
