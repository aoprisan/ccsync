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
            // PEM private key blocks. The header alone trips the scan, but
            // redaction must also swallow the key body, so the match extends
            // to the END marker when one is present. `.` (deliberately without
            // `(?s)`) spans the literal `\n` escape sequences a JSONL
            // transcript uses for embedded newlines, but never a real newline,
            // so a match cannot run from one JSONL record into another. With
            // no END marker on the line (a truncated paste), the match
            // consumes everything that can be PEM body — base64, header
            // fields, blanks, `\n`/`\r` escapes — and stops at the closing
            // JSON quote so the redacted record stays valid JSON.
            r#"-----BEGIN [A-Z ]*PRIVATE KEY-----(?:.*?-----END [A-Z ]*PRIVATE KEY-----|(?:[A-Za-z0-9+/=\t \r:,._-]|\\[nrt])*)"#,
            // JWTs (three dot-separated base64url segments starting with eyJ).
            r"eyJ[A-Za-z0-9_-]{8,}\.eyJ[A-Za-z0-9_-]{8,}\.[A-Za-z0-9_-]{8,}",
            // Generic "token"/"secret"/"password" assigned a long value. The
            // optional `\` before each quote catches JSON-escaped text nested
            // inside a transcript string (`{\"password\": \"...\"}`).
            r#"(?i)(api[_-]?key|secret|token|password)\\?["']?\s*[:=]\s*\\?["']?[A-Za-z0-9/_+\-]{24,}"#,
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
    fn redacts_whole_pem_block_in_jsonl_transcript() {
        // JSONL: the key's newlines are literal `\n` escapes inside one line.
        let body = "MIIEowIBAAKCAQEAu1SU1LfVLPHCozMxH2Mo4lgOEePzNm0tRgeLezV6ffAt0gun";
        let line = format!(
            r#"{{"type":"user","text":"-----BEGIN RSA PRIVATE KEY-----\n{body}\nAbCd+/==\n-----END RSA PRIVATE KEY-----\n","ok":"clean"}}"#
        );
        let (out, n) = redact_secrets(&line).expect("redacted");
        assert_eq!(n, 1);
        assert!(!out.contains(body), "key body survived: {out}");
        assert!(!out.contains("AbCd+/=="));
        assert!(!out.contains("END RSA"));
        assert!(out.contains(r#""ok":"clean""#));
        serde_json::from_str::<serde_json::Value>(&out).expect("still valid JSON");

        // Truncated paste without an END marker: the body is still consumed,
        // and redaction stops at the closing quote so the record stays JSON.
        let trunc = format!(r#"{{"text":"-----BEGIN PRIVATE KEY-----\n{body}\nMore","n":1}}"#);
        let (out, _) = redact_secrets(&trunc).expect("redacted");
        assert!(!out.contains(body));
        assert!(!out.contains("More"));
        serde_json::from_str::<serde_json::Value>(&out).expect("still valid JSON");

        // A header in one JSONL record never swallows the next record.
        let two =
            format!("{{\"a\":\"-----BEGIN PRIVATE KEY-----{body}\"}}\n{{\"b\":\"keep me\"}}\n");
        let (out, _) = redact_secrets(&two).expect("redacted");
        assert!(!out.contains(body));
        assert!(out.contains(r#"{"b":"keep me"}"#));

        // Scanning still trips on a bare header (raw PEM files abort).
        assert!(scan_for_secrets("-----BEGIN OPENSSH PRIVATE KEY-----").is_some());
        assert!(scan_for_secrets("-----BEGIN EC PRIVATE KEY-----\nMHcCAQEE\n").is_some());
    }

    #[test]
    fn catches_json_escaped_assignments() {
        let escaped = r#"{"text":"config: {\"password\": \"ABCDEFGHIJKLMNOPQRSTUVWXYZ012345\"}"}"#;
        assert!(scan_for_secrets(escaped).is_some());
        let (out, _) = redact_secrets(escaped).expect("redacted");
        assert!(!out.contains("ABCDEFGHIJKLMNOPQRSTUVWXYZ012345"));
        serde_json::from_str::<serde_json::Value>(&out).expect("still valid JSON");

        let escaped_key = r#"{\"api_key\":\"ABCDEFGHIJKLMNOPQRSTUVWXYZ012345\"}"#;
        assert!(scan_for_secrets(escaped_key).is_some());
    }

    #[test]
    fn detects_credential_file_by_name() {
        assert!(is_credential_file(".credentials.json"));
        assert!(is_credential_file(".claude.json"));
        assert!(!is_credential_file("settings.json"));
    }
}
