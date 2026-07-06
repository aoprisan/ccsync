//! GitHub Copilot CLI support. Copilot keeps its state in `~/.copilot`
//! (overridable via `COPILOT_HOME`), a close analog of `~/.claude`: portable
//! config and customization, session state that embeds absolute paths, and
//! credential files that must never leave the machine.
//!
//! The Copilot tree rides inside snapshots under the reserved
//! [`COMPONENT`] name — `data/ccsync-copilot/…` — exactly like the profile
//! store rides under `ccsync-profiles/`. The Claude layout is untouched, so
//! diff/machines/layers/profiles keep working unchanged, and `restore --only
//! copilot` selects the whole Copilot subtree through the ordinary component
//! filter. On restore, `apply_tree` routes the component into the local
//! Copilot directory instead of `~/.claude`.
//!
//! Unlike Claude's `projects/<encoded-cwd>` layout, Copilot keys session
//! directories by session ID; only file *contents* (session events,
//! `permissions-config.json`'s absolute-path keys) embed machine paths, so
//! remapping rewrites content without renaming directories.

use std::path::Path;

use crate::config::Config;

/// Reserved top-level snapshot component carrying the Copilot tree.
/// The `ccsync-` prefix keeps it from colliding with a real `~/.claude` entry,
/// mirroring `ccsync-profiles`.
pub const COMPONENT: &str = "ccsync-copilot";

/// User-facing alias accepted by `restore --only` for [`COMPONENT`].
pub const ONLY_ALIAS: &str = "copilot";

/// Include entries gated by `[copilot] include_sessions`.
pub const SESSION_ENTRIES: &[&str] = &["session-state", "command-history-state"];

/// Credential files under `~/.copilot` that must never be captured,
/// regardless of configuration. Checked against copilot-root-relative paths:
/// `config.json` only at the root (it stores auth tokens there, while a nested
/// `skills/foo/config.json` is ordinary data); the keychain-fallback secret
/// stores block their whole subtrees at any depth.
const ROOT_BLOCKED: &[&str] = &["config.json"];
const NAME_BLOCKED: &[&str] = &["mcp-oauth-config", "mcp-secrets"];

/// Check `rel` (a copilot-root-relative path with forward slashes) against the
/// credential hard-block list. Returns the matched entry for the error message.
pub fn credential_block_match(rel: &str) -> Option<&'static str> {
    let root_hit = ROOT_BLOCKED
        .iter()
        .find(|root| rel == **root || rel.starts_with(&format!("{root}/")));
    let name_hit = || {
        NAME_BLOCKED
            .iter()
            .find(|name| rel.split('/').any(|component| component == **name))
    };
    root_hit.or_else(name_hit).copied()
}

/// Top-level entries of `~/.copilot` matched by neither the copilot `include`
/// nor `exclude` lists — surfaced like `snapshot::unclassified_top_level` so
/// new Copilot state is classified instead of silently dropped.
pub fn unclassified_top_level(copilot_dir: &Path, config: &Config) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(copilot_dir) else {
        return Vec::new();
    };
    let mut out: Vec<String> = entries
        .flatten()
        .filter_map(|e| e.file_name().to_str().map(str::to_string))
        .filter(|name| {
            !config
                .copilot
                .include
                .iter()
                .any(|i| i.trim_end_matches('/') == name)
                && !config.copilot.is_excluded(name)
        })
        .collect();
    out.sort();
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blocks_root_config_but_not_nested() {
        assert_eq!(credential_block_match("config.json"), Some("config.json"));
        assert!(credential_block_match("skills/my-skill/config.json").is_none());
        assert!(credential_block_match("settings.json").is_none());
        assert!(credential_block_match("mcp-config.json").is_none());
    }

    #[test]
    fn blocks_secret_dirs_at_any_depth() {
        assert!(credential_block_match("mcp-oauth-config/token.json").is_some());
        assert!(credential_block_match("mcp-secrets/index.json").is_some());
        assert!(credential_block_match("nested/mcp-secrets/x").is_some());
    }

    #[test]
    fn unclassified_surfaces_unknown_entries() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("settings.json"), "{}").unwrap();
        std::fs::write(tmp.path().join("config.json"), "{}").unwrap();
        std::fs::create_dir(tmp.path().join("brand-new-dir")).unwrap();
        let config = Config::default();
        assert_eq!(
            unclassified_top_level(tmp.path(), &config),
            vec!["brand-new-dir".to_string()]
        );
    }
}
