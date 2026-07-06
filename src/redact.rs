//! Secret detection. Before a text config file (settings.json, an optional
//! mcpServers blob) is captured, we scan it for strings that look like API
//! keys or tokens and abort unless the user explicitly opts in with
//! `--allow-secrets`. This is a best-effort guard, not a guarantee — it exists
//! to stop the obvious foot-guns (a literal API key pasted into settings).
//!
//! Credential *files* are handled separately as a hard block in `snapshot`:
//! each tool's [`crate::tools::ToolSpec`] carries a blocklist checked here by
//! [`credential_block_match`]. Blocked files are never scanned because they
//! are never captured.

use std::sync::OnceLock;

use regex::Regex;

use crate::tools::{CredentialBlock, ToolSpec};

/// Check `rel` (a root-relative path with forward slashes) against a tool's
/// credential hard-block list. Returns the matched blocklist entry so the
/// caller can name it in the abort error.
pub fn credential_block_match(spec: &ToolSpec, rel: &str) -> Option<String> {
    for block in spec.credential_blocklist {
        match block {
            CredentialBlock::Name(name) => {
                if rel.split('/').any(|component| component == *name) {
                    return Some((*name).to_string());
                }
            }
            CredentialBlock::Root(root) => {
                if rel == *root || rel.starts_with(&format!("{root}/")) {
                    return Some((*root).to_string());
                }
            }
        }
    }
    None
}

fn secret_patterns() -> &'static [Regex] {
    static PATTERNS: OnceLock<Vec<Regex>> = OnceLock::new();
    PATTERNS.get_or_init(|| {
        [
            // Anthropic / OpenAI style keys.
            r"sk-[A-Za-z0-9_-]{16,}",
            // GitHub tokens.
            r"gh[pousr]_[A-Za-z0-9]{20,}",
            // AWS access key id.
            r"AKIA[0-9A-Z]{16}",
            // Generic "token"/"secret"/"password" assigned a long value.
            r#"(?i)(api[_-]?key|secret|token|password)["']?\s*[:=]\s*["']?[A-Za-z0-9/_+\-]{24,}"#,
        ]
        .iter()
        .map(|p| Regex::new(p).expect("static regex compiles"))
        .collect()
    })
}

/// Scan `content` for secret-shaped substrings. Returns a short human-readable
/// hint for the first match found, or `None` if nothing looked sensitive.
pub fn scan_for_secrets(content: &str) -> Option<String> {
    for re in secret_patterns() {
        if let Some(m) = re.find(content) {
            let matched = m.as_str();
            // Truncate so we never echo the full secret back to the terminal.
            let shown: String = matched.chars().take(8).collect();
            return Some(format!("matched pattern near \"{shown}…\""));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::{spec, ToolId};

    #[test]
    fn flags_obvious_keys() {
        assert!(scan_for_secrets("sk-abcdefghijklmnopqrstuvwx").is_some());
        assert!(scan_for_secrets("ghp_0123456789abcdefghij0123").is_some());
        assert!(scan_for_secrets(r#"{"api_key": "ABCDEFGHIJKLMNOPQRSTUVWXYZ012345"}"#).is_some());
    }

    #[test]
    fn passes_clean_settings() {
        let clean = r#"{"model": "claude-opus-4-8", "theme": "dark"}"#;
        assert!(scan_for_secrets(clean).is_none());
    }

    #[test]
    fn claude_blocks_credentials_at_any_depth() {
        let s = spec(ToolId::Claude);
        assert!(credential_block_match(s, ".credentials.json").is_some());
        assert!(credential_block_match(s, "backups/old/.credentials.json").is_some());
        assert!(credential_block_match(s, "settings.json").is_none());
    }

    #[test]
    fn copilot_blocks_root_config_but_not_nested() {
        let s = spec(ToolId::Copilot);
        // `~/.copilot/config.json` holds auth tokens.
        assert!(credential_block_match(s, "config.json").is_some());
        // A skill's own config.json is not a credential file.
        assert!(credential_block_match(s, "skills/my-skill/config.json").is_none());
    }

    #[test]
    fn copilot_blocks_secret_dirs_recursively() {
        let s = spec(ToolId::Copilot);
        assert!(credential_block_match(s, "mcp-oauth-config/token.json").is_some());
        assert!(credential_block_match(s, "mcp-secrets/index.json").is_some());
        assert!(credential_block_match(s, "mcp-config.json").is_none());
    }
}
