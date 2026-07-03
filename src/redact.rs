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
            // Slack tokens.
            r"xox[baprs]-[A-Za-z0-9-]{10,}",
            // Google API keys.
            r"AIza[0-9A-Za-z_-]{35}",
            // PEM private key headers.
            r"-----BEGIN [A-Z ]*PRIVATE KEY-----",
            // JWTs (three dot-separated base64url segments starting with eyJ).
            r"eyJ[A-Za-z0-9_-]{8,}\.eyJ[A-Za-z0-9_-]{8,}\.[A-Za-z0-9_-]{8,}",
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

/// Marker written in place of a redacted secret.
pub const REDACTION_MARKER: &str = "[REDACTED:ccsync]";

/// Replace every secret-shaped span in `content` with [`REDACTION_MARKER`].
/// Returns the rewritten text and the number of spans replaced, or `None`
/// when nothing matched. Used for session transcripts, where aborting on a
/// discussed secret would make snapshots unusable.
pub fn redact_secrets(content: &str) -> Option<(String, usize)> {
    let mut spans: Vec<std::ops::Range<usize>> = secret_patterns()
        .iter()
        .flat_map(|re| re.find_iter(content).map(|m| m.range()))
        .collect();
    if spans.is_empty() {
        return None;
    }
    spans.sort_by_key(|r| (r.start, r.end));
    // Merge overlapping matches from different patterns.
    let mut merged: Vec<std::ops::Range<usize>> = Vec::with_capacity(spans.len());
    for span in spans {
        match merged.last_mut() {
            Some(last) if span.start <= last.end => last.end = last.end.max(span.end),
            _ => merged.push(span),
        }
    }
    let mut out = String::with_capacity(content.len());
    let mut cursor = 0;
    for span in &merged {
        out.push_str(&content[cursor..span.start]);
        out.push_str(REDACTION_MARKER);
        cursor = span.end;
    }
    out.push_str(&content[cursor..]);
    Some((out, merged.len()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flags_obvious_keys() {
        assert!(scan_for_secrets("sk-abcdefghijklmnopqrstuvwx").is_some());
        assert!(scan_for_secrets("ghp_0123456789abcdefghij0123").is_some());
        assert!(scan_for_secrets(r#"{"api_key": "ABCDEFGHIJKLMNOPQRSTUVWXYZ012345"}"#).is_some());
        assert!(scan_for_secrets("xoxb-123456789012-abcdefghij").is_some());
        assert!(scan_for_secrets("AIzaSyA1234567890abcdefghijklmnopqrstuv").is_some());
        assert!(scan_for_secrets("-----BEGIN RSA PRIVATE KEY-----").is_some());
        assert!(scan_for_secrets("-----BEGIN PRIVATE KEY-----").is_some());
        assert!(scan_for_secrets(
            "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjM0In0.SflKxwRJSMeKKF2QT4fwpM"
        )
        .is_some());
    }

    #[test]
    fn passes_clean_settings() {
        let clean = r#"{"model": "claude-opus-4-8", "theme": "dark"}"#;
        assert!(scan_for_secrets(clean).is_none());
    }

    #[test]
    fn redacts_secrets_in_place() {
        let text = r#"{"cwd":"/home/x","paste":"sk-abcdefghijklmnopqrstuvwx","ok":"clean"}"#;
        let (out, n) = redact_secrets(text).expect("one span");
        assert_eq!(n, 1);
        assert!(!out.contains("sk-abcdefghijklmnopqrstuvwx"));
        assert!(out.contains(REDACTION_MARKER));
        // Untouched content survives byte-for-byte.
        assert!(out.contains(r#""cwd":"/home/x""#));
        assert!(out.contains(r#""ok":"clean""#));

        // Clean text returns None.
        assert!(redact_secrets(r#"{"theme":"dark"}"#).is_none());

        // Overlapping matches (generic assignment + key pattern) collapse to
        // one marker instead of corrupting the text.
        let overlap = r#"api_key = "sk-abcdefghijklmnopqrstuvwx""#;
        let (out, _) = redact_secrets(overlap).expect("redacted");
        assert!(!out.contains("sk-abcdef"));
    }

    #[test]
    fn detects_credential_file_by_name() {
        assert!(is_credential_file(".credentials.json"));
        assert!(!is_credential_file("settings.json"));
    }
}
