//! Secret detection. Before a text config file (settings.json, an optional
//! mcpServers blob) is captured, we scan it for strings that look like API
//! keys or tokens and abort unless the user explicitly opts in with
//! `--allow-secrets`. This is a best-effort guard, not a guarantee — it exists
//! to stop the obvious foot-guns (a literal API key pasted into settings).
//!
//! Credential *files* (`.credentials.json`) are handled separately as a hard
//! block in `snapshot`; they are never scanned because they are never captured.

use std::sync::OnceLock;

use regex::Regex;

use crate::config::CREDENTIAL_BLOCKLIST;

/// Returns the file name if `name` is on the credential blocklist.
pub fn is_credential_file(name: &str) -> bool {
    CREDENTIAL_BLOCKLIST.contains(&name)
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
    fn detects_credential_file_by_name() {
        assert!(is_credential_file(".credentials.json"));
        assert!(!is_credential_file("settings.json"));
    }
}
