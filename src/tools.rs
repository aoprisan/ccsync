//! The multi-tool source model. ccsync can capture more than one AI coding
//! tool's state; each supported tool is described by a static [`ToolSpec`]
//! (identity, credential hard-block list, remap strategy — things that are
//! *not* user policy) while the per-tool include/exclude policy lives in
//! [`crate::config::Config`]. A [`ToolPlan`] joins the two with the resolved
//! source root for one run; `snapshot`/`restore` iterate plans instead of
//! assuming a single `~/.claude`.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::config::Config;
use crate::paths;

/// Identity of a supported tool. `Default` is `Claude` on purpose: v1
/// manifests predate per-file tool tags, so untagged entries must
/// deserialize as Claude.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default, clap::ValueEnum,
)]
#[serde(rename_all = "lowercase")]
pub enum ToolId {
    #[default]
    Claude,
    Copilot,
}

impl ToolId {
    /// Stable lowercase name, used for staging subdirectories
    /// (`data/<tool>/`) and CLI values.
    pub fn as_str(self) -> &'static str {
        match self {
            ToolId::Claude => "claude",
            ToolId::Copilot => "copilot",
        }
    }

    pub fn all() -> &'static [ToolId] {
        &[ToolId::Claude, ToolId::Copilot]
    }
}

impl std::fmt::Display for ToolId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// How restored session content is made findable on the target machine.
pub enum RemapStrategy {
    /// Rewrite `projects/**/*.jsonl` contents, then rename the dash-encoded
    /// `projects/<encoded>` directories (Claude Code's layout).
    ClaudeProjects,
    /// Rewrite absolute-path prefixes inside `*.json`/`*.jsonl` contents
    /// anywhere in the tool subtree; no directory renaming (Copilot keys its
    /// session dirs by session ID, not by encoded cwd).
    ContentOnly,
}

/// One entry in a tool's credential hard-block list. Matching is against the
/// root-relative path of a candidate file, enforced in the capture path
/// independent of configuration.
pub enum CredentialBlock {
    /// Matches a file or directory *name* at any depth; a matching directory
    /// blocks its whole subtree.
    Name(&'static str),
    /// Matches only this exact root-relative path (or anything under it).
    /// Used when a name is a credential file at the tool root but legitimate
    /// elsewhere (Copilot's `config.json`).
    Root(&'static str),
}

/// Static, config-independent description of a supported tool.
pub struct ToolSpec {
    /// Credentials never leave the machine: candidate files matching any of
    /// these abort the snapshot regardless of include/exclude configuration.
    pub credential_blocklist: &'static [CredentialBlock],
    pub remap: RemapStrategy,
    /// Include entries additionally gated by the tool's `include_sessions`
    /// config flag.
    pub session_entries: &'static [&'static str],
}

static CLAUDE_SPEC: ToolSpec = ToolSpec {
    credential_blocklist: &[CredentialBlock::Name(".credentials.json")],
    remap: RemapStrategy::ClaudeProjects,
    session_entries: &["projects"],
};

static COPILOT_SPEC: ToolSpec = ToolSpec {
    credential_blocklist: &[
        // `~/.copilot/config.json` holds auth tokens (possibly plaintext);
        // a nested `skills/foo/config.json` is fine, hence Root not Name.
        CredentialBlock::Root("config.json"),
        // Keychain-fallback MCP OAuth tokens / secret stores.
        CredentialBlock::Name("mcp-oauth-config"),
        CredentialBlock::Name("mcp-secrets"),
    ],
    remap: RemapStrategy::ContentOnly,
    session_entries: &["session-state", "command-history-state"],
};

/// The static spec for a tool.
pub fn spec(id: ToolId) -> &'static ToolSpec {
    match id {
        ToolId::Claude => &CLAUDE_SPEC,
        ToolId::Copilot => &COPILOT_SPEC,
    }
}

/// A tool resolved for one run: source root plus effective policy.
pub struct ToolPlan {
    pub id: ToolId,
    /// The tool's data directory on this machine (e.g. `~/.claude`). May not
    /// exist; snapshot then captures nothing for the tool.
    pub root: PathBuf,
    pub include: Vec<String>,
    pub exclude: Vec<String>,
    pub include_sessions: bool,
}

impl ToolPlan {
    /// Build a plan for `id` rooted at `root`, pulling policy from `config`.
    pub fn for_tool(id: ToolId, root: PathBuf, config: &Config) -> Self {
        match id {
            ToolId::Claude => ToolPlan {
                id,
                root,
                include: config.include.clone(),
                exclude: config.exclude.clone(),
                include_sessions: config.include_sessions,
            },
            ToolId::Copilot => ToolPlan {
                id,
                root,
                include: config.copilot.include.clone(),
                exclude: config.copilot.exclude.clone(),
                include_sessions: config.copilot.include_sessions,
            },
        }
    }

    /// True if `rel` (root-relative, forward slashes) is excluded by any
    /// configured exclude prefix.
    pub fn is_excluded(&self, rel: &str) -> bool {
        is_excluded_by(&self.exclude, rel)
    }
}

/// Prefix-exclusion check shared with `Config::is_excluded`.
pub fn is_excluded_by(exclude: &[String], rel: &str) -> bool {
    let rel = rel.replace('\\', "/");
    exclude.iter().any(|ex| {
        let ex = ex.trim_end_matches('/');
        rel == ex || rel.starts_with(&format!("{ex}/"))
    })
}

/// Resolve the enabled tools for this run. Claude is always enabled; Copilot
/// is gated by `[copilot] enabled`. A tool whose root directory does not
/// exist still gets a plan — snapshot simply captures nothing for it.
pub fn plans(config: &Config) -> anyhow::Result<Vec<ToolPlan>> {
    let mut out = Vec::new();
    for &id in ToolId::all() {
        if id == ToolId::Copilot && !config.copilot.enabled {
            continue;
        }
        let root = paths::tool_dir(id)?;
        out.push(ToolPlan::for_tool(id, root, config));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_ids_are_stable_lowercase() {
        assert_eq!(ToolId::Claude.as_str(), "claude");
        assert_eq!(ToolId::Copilot.as_str(), "copilot");
        // v1-manifest back-compat hinges on this default.
        assert_eq!(ToolId::default(), ToolId::Claude);
    }

    #[test]
    fn plans_respect_copilot_enabled_flag() {
        let mut config = Config::default();
        let all = plans(&config).unwrap();
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].id, ToolId::Claude);
        assert_eq!(all[1].id, ToolId::Copilot);

        config.copilot.enabled = false;
        let claude_only = plans(&config).unwrap();
        assert_eq!(claude_only.len(), 1);
        assert_eq!(claude_only[0].id, ToolId::Claude);
    }

    #[test]
    fn plan_pulls_policy_per_tool() {
        let config = Config::default();
        let claude = ToolPlan::for_tool(ToolId::Claude, "/x".into(), &config);
        assert!(claude.include.iter().any(|e| e == "projects"));
        assert!(claude.is_excluded("shell-snapshots/s.sh"));

        let copilot = ToolPlan::for_tool(ToolId::Copilot, "/y".into(), &config);
        assert!(copilot.include.iter().any(|e| e == "session-state"));
        assert!(copilot.is_excluded("logs/process-1-2.log"));
        assert!(copilot.is_excluded("session-store.db"));
        assert!(!copilot.is_excluded("skills/my-skill/SKILL.md"));
    }
}
