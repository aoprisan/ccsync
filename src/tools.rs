//! Other coding agents whose state ccsync carries alongside Claude Code:
//! OpenAI's Codex CLI (`$CODEX_HOME`, default `~/.codex`) and GitHub's
//! Copilot CLI (`$COPILOT_HOME`, default `~/.copilot`).
//!
//! Each tool's home is captured under a reserved top-level component of the
//! snapshot (`ccsync-codex/`, `ccsync-copilot/`) and routed back to the local
//! home on restore, through the same integrity check, secret scan, remap,
//! symlink guard and hook gate as `~/.claude`. Tools are opt-in
//! (`[codex] enabled = true`) so upgrading never changes what an existing
//! backup carries.
//!
//! The same invariants as Claude Code's state apply:
//! - credentials never leave the machine: Codex's `auth.json` and Copilot's
//!   `config.json` (managed app state holding auth) are hard-blocked
//!   regardless of include/exclude, in both directions;
//! - trust decisions stay local: Codex's `[projects."<path>"]` trust table is
//!   stripped from the staged `config.toml`, and Copilot's
//!   `permissions-config.json` is excluded;
//! - config that makes the tool run code (hooks, MCP/LSP servers, `notify`,
//!   exec-policy rules, approval/sandbox modes, hook scripts) is gated on
//!   restore exactly like Claude Code hooks.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde_json::Value;
use walkdir::WalkDir;

use crate::config::{Config, ToolConfig};

/// A supported non-Claude coding agent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Tool {
    Codex,
    Copilot,
}

/// A tool together with the local directory holding its state.
#[derive(Debug, Clone)]
pub struct ToolHome {
    pub tool: Tool,
    pub dir: PathBuf,
}

impl Tool {
    pub const ALL: [Tool; 2] = [Tool::Codex, Tool::Copilot];

    /// Short name, as used in config tables and `restore --only`.
    pub fn name(self) -> &'static str {
        match self {
            Tool::Codex => "codex",
            Tool::Copilot => "copilot",
        }
    }

    /// Reserved top-level snapshot component holding this tool's tree.
    pub fn component(self) -> &'static str {
        match self {
            Tool::Codex => "ccsync-codex",
            Tool::Copilot => "ccsync-copilot",
        }
    }

    /// The tool whose reserved component is `top`.
    pub fn from_component(top: &str) -> Option<Tool> {
        Tool::ALL.into_iter().find(|t| t.component() == top)
    }

    /// The tool named `s` by its short name or its component.
    pub fn from_alias(s: &str) -> Option<Tool> {
        Tool::ALL
            .into_iter()
            .find(|t| t.name() == s || t.component() == s)
    }

    /// Environment variable that relocates the tool's home.
    pub fn home_env(self) -> &'static str {
        match self {
            Tool::Codex => "CODEX_HOME",
            Tool::Copilot => "COPILOT_HOME",
        }
    }

    /// Home directory name under `~` when the env var is unset.
    pub fn default_dir_name(self) -> &'static str {
        match self {
            Tool::Codex => ".codex",
            Tool::Copilot => ".copilot",
        }
    }

    /// Home-relative files holding credentials. Hard-blocked: never captured,
    /// never restored, whatever the include/exclude config says.
    pub fn credential_files(self) -> &'static [&'static str] {
        match self {
            // API key or ChatGPT OAuth tokens.
            Tool::Codex => &["auth.json"],
            // Automatically managed app state: logged-in users and their auth
            // (stored here in plaintext when no keychain is available).
            Tool::Copilot => &["config.json"],
        }
    }

    /// Default top-level entries captured from the tool's home.
    pub fn default_include(self) -> &'static [&'static str] {
        match self {
            Tool::Codex => &[
                "config.toml",
                "AGENTS.md",
                "AGENTS.override.md",
                "prompts",
                "skills",
                "agents",
                "rules",
                "hooks.json",
                "sessions",
                "archived_sessions",
                "history.jsonl",
            ],
            Tool::Copilot => &[
                "settings.json",
                "mcp-config.json",
                "lsp-config.json",
                "copilot-instructions.md",
                "instructions",
                "agents",
                "skills",
                "hooks",
                "session-state",
                "history-session-state",
            ],
        }
    }

    /// Default exclusions: credentials, machine-local trust, caches, logs and
    /// databases. `*` matches within one path component.
    pub fn default_exclude(self) -> &'static [&'static str] {
        match self {
            Tool::Codex => &[
                "auth.json",
                "log",
                "*.sqlite",
                "*.sqlite-*",
                "shell_snapshots",
                "plugins/cache",
                ".tmp",
                "tmp",
                "models_cache.json",
                "version.json",
                "installation_id",
                ".sandbox",
                ".sandbox-bin",
                ".sandbox-secrets",
            ],
            Tool::Copilot => &[
                "config.json",
                "permissions-config.json",
                "command-history-state.json",
                "logs",
                "pkg",
            ],
        }
    }

    /// Top-level entries that hold session history: skipped when
    /// `include_sessions` is off, and given the transcript secret policy
    /// (redact, not abort) and path remapping.
    pub fn session_entries(self) -> &'static [&'static str] {
        match self {
            Tool::Codex => &["sessions", "archived_sessions", "history.jsonl"],
            Tool::Copilot => &["session-state", "history-session-state"],
        }
    }

    /// True when the home-relative `rel` is a credential file.
    pub fn is_credential(self, rel: &str) -> bool {
        self.credential_files()
            .contains(&rel.trim_start_matches("./"))
    }

    /// True when the home-relative `rel` lies under a session entry.
    pub fn is_session_path(self, rel: &str) -> bool {
        let top = rel.split('/').next().unwrap_or(rel);
        self.session_entries().contains(&top)
    }
}

/// Resolve every tool's local home. `enabled_only` keeps just the tools the
/// config enables (capture); restore routes whatever a snapshot carries.
pub fn homes(config: &Config, enabled_only: bool) -> Vec<ToolHome> {
    Tool::ALL
        .into_iter()
        .filter(|t| !enabled_only || config.tool(*t).enabled)
        .filter_map(|tool| {
            crate::paths::tool_home(tool)
                .ok()
                .map(|dir| ToolHome { tool, dir })
        })
        .collect()
}

/// Top-level entries of a tool's home matched by neither its include nor its
/// exclude list (silently not captured; surfaced like `~/.claude`'s).
pub fn unclassified_top_level(home: &ToolHome, tc: &ToolConfig) -> Vec<String> {
    let Ok(entries) = fs::read_dir(&home.dir) else {
        return Vec::new();
    };
    let include = tc.include_for(home.tool);
    let mut out: Vec<String> = entries
        .flatten()
        .filter_map(|e| e.file_name().to_str().map(str::to_string))
        .filter(|name| {
            !include.iter().any(|i| i.trim_end_matches('/') == name)
                && !tc.is_excluded(home.tool, name)
        })
        .collect();
    out.sort();
    out
}

/// `*`-glob match of one path component (`*` matches any run of characters).
pub(crate) fn glob_component(pattern: &str, s: &str) -> bool {
    let parts: Vec<&str> = pattern.split('*').collect();
    if parts.len() == 1 {
        return pattern == s;
    }
    let (first, last) = (parts[0], parts[parts.len() - 1]);
    if !s.starts_with(first) || s.len() < first.len() + last.len() || !s.ends_with(last) {
        return false;
    }
    let mut rest = &s[first.len()..s.len() - last.len()];
    for mid in &parts[1..parts.len() - 1] {
        match rest.find(mid) {
            Some(i) => rest = &rest[i + mid.len()..],
            None => return false,
        }
    }
    true
}

/// The staged form of a captured file, when it must differ from the source.
/// Codex's `config.toml` loses its `[projects]` table: those are per-path
/// trust decisions for this machine and must not grant trust elsewhere.
/// `None` keeps the bytes as they are.
pub fn staged_bytes(tool: Tool, rel: &str, bytes: &[u8]) -> Option<Vec<u8>> {
    if tool != Tool::Codex || rel != "config.toml" {
        return None;
    }
    let mut doc: toml::Table = std::str::from_utf8(bytes).ok()?.parse().ok()?;
    doc.remove("projects")?;
    Some(toml::to_string_pretty(&doc).ok()?.into_bytes())
}

/// Deep-merge the TOML at `incoming` into `existing` (tables merge key by
/// key, scalar arrays union, anything else from `incoming` wins) — the TOML
/// counterpart of restore's JSON merge, for Codex's `config.toml`.
pub fn merge_toml_file(incoming: &Path, existing: &Path) -> Result<()> {
    let inc: toml::Value = fs::read_to_string(incoming)?
        .parse::<toml::Table>()
        .map(toml::Value::Table)
        .with_context(|| format!("parsing {}", incoming.display()))?;
    let mut base: toml::Value = fs::read_to_string(existing)?
        .parse::<toml::Table>()
        .map(toml::Value::Table)
        .unwrap_or_else(|_| toml::Value::Table(Default::default()));
    merge_toml(&mut base, inc);
    fs::write(existing, toml::to_string_pretty(&base)?)
        .with_context(|| format!("writing {}", existing.display()))?;
    Ok(())
}

fn merge_toml(base: &mut toml::Value, incoming: toml::Value) {
    use toml::Value as T;
    let scalar = |v: &T| !matches!(v, T::Table(_) | T::Array(_));
    match (base, incoming) {
        (T::Table(b), T::Table(i)) => {
            for (k, v) in i {
                match b.get_mut(&k) {
                    Some(existing) => merge_toml(existing, v),
                    None => {
                        b.insert(k, v);
                    }
                }
            }
        }
        (T::Array(b), T::Array(i)) if b.iter().all(scalar) && i.iter().all(scalar) => {
            for v in i {
                if !b.contains(&v) {
                    b.push(v);
                }
            }
        }
        (b, i) => *b = i,
    }
}

/// Parse a config file of the tool (JSON or TOML) into a JSON value for
/// inspection; `None` when absent or unparseable.
fn read_doc(path: &Path) -> Option<Value> {
    let text = fs::read_to_string(path).ok()?;
    if path.extension().and_then(|e| e.to_str()) == Some("toml") {
        let table: toml::Table = text.parse().ok()?;
        serde_json::to_value(table).ok()
    } else {
        serde_json::from_str(&text).ok()
    }
}

/// Config files of `tool` (home-relative) that can make it run code, and
/// whose executables are extracted from their parsed content.
fn executable_configs(tool: Tool) -> &'static [&'static str] {
    match tool {
        Tool::Codex => &["config.toml", "hooks.json"],
        Tool::Copilot => &["settings.json", "mcp-config.json", "lsp-config.json"],
    }
}

/// Codex settings that loosen what it may do without asking.
const CODEX_POLICY_KEYS: &[&str] = &["approval_policy", "sandbox_mode"];

/// Everything in a parsed config file that makes the tool run code or widens
/// what it may run, each entry prefixed with the file it came from.
fn executables_in(tool: Tool, rel: &str, doc: &Value) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    let mut cmds = BTreeSet::new();
    match (tool, rel) {
        (Tool::Codex, "config.toml") => {
            if let Some(notify) = doc.get("notify") {
                out.insert(format!("{rel}: notify = {}", join_command(notify)));
            }
            collect_commands(doc.get("mcp_servers").unwrap_or(&Value::Null), &mut cmds);
            let profiles = doc
                .get("profiles")
                .and_then(Value::as_object)
                .into_iter()
                .flat_map(|p| p.iter().map(|(n, v)| (format!("profiles.{n}."), v)));
            for (prefix, table) in std::iter::once((String::new(), doc)).chain(profiles) {
                for key in CODEX_POLICY_KEYS {
                    if let Some(v) = table.get(*key) {
                        out.insert(format!("{rel}: {prefix}{key} = {v}"));
                    }
                }
            }
            if let Some(Value::Object(set)) = doc.pointer("/shell_environment_policy/set") {
                for (k, v) in set {
                    out.insert(format!("{rel}: env {k}={}", plain(v)));
                }
            }
        }
        (Tool::Copilot, "settings.json") => {
            collect_commands(doc.get("hooks").unwrap_or(&Value::Null), &mut cmds);
        }
        _ => collect_commands(doc, &mut cmds),
    }
    out.extend(cmds.into_iter().map(|c| format!("{rel}: {c}")));
    out
}

fn plain(v: &Value) -> String {
    v.as_str()
        .map(str::to_string)
        .unwrap_or_else(|| v.to_string())
}

fn join_command(v: &Value) -> String {
    match v {
        Value::Array(items) => items.iter().map(plain).collect::<Vec<_>>().join(" "),
        other => plain(other),
    }
}

/// Every command in a hooks/MCP/LSP document: objects with a `command`
/// (string, or argv array) plus their `args`, and hook entries' `bash` /
/// `powershell` script strings.
fn collect_commands(v: &Value, out: &mut BTreeSet<String>) {
    match v {
        Value::Object(map) => {
            if let Some(cmd) = map.get("command").filter(|c| c.is_string() || c.is_array()) {
                let mut line = join_command(cmd);
                if let Some(Value::Array(args)) = map.get("args") {
                    for a in args {
                        line.push(' ');
                        line.push_str(&plain(a));
                    }
                }
                out.insert(line);
            }
            for key in ["bash", "powershell"] {
                if let Some(Value::String(script)) = map.get(key) {
                    out.insert(format!("{key}: {script}"));
                }
            }
            for child in map.values() {
                collect_commands(child, out);
            }
        }
        Value::Array(items) => items.iter().for_each(|c| collect_commands(c, out)),
        _ => {}
    }
}

/// Non-blank, non-comment lines of a Codex exec-policy `.rules` file.
fn rule_lines(path: &Path) -> BTreeSet<String> {
    fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(str::to_string)
        .collect()
}

/// Commands and permission widenings that restoring the staged tree `src`
/// (a snapshot's `ccsync-<tool>/`) into `home` would newly introduce,
/// prefixed with the tool name. Merged config files are compared after a
/// simulated merge (so locally present entries don't re-prompt); copied
/// files are compared as they will land: new exec-policy rule lines, and any
/// new or changed file under Copilot's `hooks/` (hook scripts are code).
pub fn new_executables(
    tool: Tool,
    src: &Path,
    home: &Path,
    overwrite: bool,
) -> Result<BTreeSet<String>> {
    let mut out = BTreeSet::new();
    let tag = tool.name();
    for rel in executable_configs(tool) {
        let incoming_path = src.join(rel);
        if !incoming_path.is_file() {
            continue;
        }
        let Some(incoming) = read_doc(&incoming_path) else {
            continue;
        };
        let local = read_doc(&home.join(rel));
        let landed = match (&local, overwrite) {
            (Some(local), false) => {
                let mut merged = local.clone();
                crate::restore::merge_value(&mut merged, incoming);
                merged
            }
            _ => incoming,
        };
        let before = local
            .map(|l| executables_in(tool, rel, &l))
            .unwrap_or_default();
        out.extend(
            executables_in(tool, rel, &landed)
                .difference(&before)
                .map(|e| format!("{tag} {e}")),
        );
    }
    for entry in WalkDir::new(src).follow_links(false) {
        let entry = entry?;
        if !entry.file_type().is_file() {
            continue;
        }
        let rel = entry
            .path()
            .strip_prefix(src)
            .expect("walk stays under src")
            .to_string_lossy()
            .replace('\\', "/");
        let local = home.join(&rel);
        match tool {
            Tool::Codex if rel.starts_with("rules/") => {
                let before = rule_lines(&local);
                for line in rule_lines(entry.path()).difference(&before) {
                    out.insert(format!("{tag} {rel}: {line}"));
                }
            }
            Tool::Copilot if rel.starts_with("hooks/") => {
                let same = fs::read(&local).ok() == Some(fs::read(entry.path())?);
                if !same {
                    out.insert(format!("{tag} {rel}: new or changed hook file"));
                }
            }
            _ => {}
        }
    }
    Ok(out)
}

/// Copy every file of `home` that applying the staged tree `src` would
/// overwrite into a fresh `<home>.ccsync-backup-<ts>` sibling, preserving
/// relative paths. Only overwritten files are copied (a tool home holds large
/// session trees and live databases); files the restore merely adds need no
/// backup. Returns the backup dir, or `None` when nothing would be replaced.
pub fn backup_overwrites(src: &Path, home: &Path) -> Result<Option<PathBuf>> {
    let mut backup = None;
    for entry in WalkDir::new(src).follow_links(false) {
        let entry = entry?;
        if !entry.file_type().is_file() {
            continue;
        }
        let rel = entry
            .path()
            .strip_prefix(src)
            .expect("walk stays under src");
        let existing = home.join(rel);
        // Only regular files: a link on the path is skipped by apply anyway.
        if !fs::symlink_metadata(&existing).is_ok_and(|m| m.is_file()) {
            continue;
        }
        let root = match &backup {
            Some(root) => root,
            None => backup.insert(crate::restore::backup_path(home, ".tool-home")),
        };
        let dest = root.join(rel);
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::copy(&existing, &dest).with_context(|| format!("backing up {}", existing.display()))?;
    }
    Ok(backup)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(path: &Path, content: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
    }

    #[test]
    fn credentials_are_excluded_by_default_and_hard_blocked() {
        for tool in Tool::ALL {
            let tc = ToolConfig::default();
            for cred in tool.credential_files() {
                assert!(tool.is_credential(cred));
                assert!(tc.is_excluded(tool, cred), "{tool:?} {cred}");
                assert!(!tool.default_include().contains(cred));
            }
        }
        assert!(!Tool::Codex.is_credential("skills/x/auth.json"));
    }

    #[test]
    fn glob_excludes_match_one_component() {
        let tc = ToolConfig::default();
        assert!(tc.is_excluded(Tool::Codex, "state_5.sqlite"));
        assert!(tc.is_excluded(Tool::Codex, "logs_2.sqlite-wal"));
        assert!(tc.is_excluded(Tool::Codex, "plugins/cache/x/y"));
        assert!(!tc.is_excluded(Tool::Codex, "plugins/other"));
        assert!(!tc.is_excluded(Tool::Codex, "sessions/2026/01/01/rollout-a.jsonl"));
        assert!(glob_component("a*b*c", "aXbYc"));
        assert!(!glob_component("a*b", "ab-c"));
    }

    #[test]
    fn strips_codex_trust_table_only() {
        let src = "model = \"o3\"\n\n[projects.\"/Users/a/p\"]\ntrust_level = \"trusted\"\n\n[mcp_servers.x]\ncommand = \"npx\"\n";
        let out =
            String::from_utf8(staged_bytes(Tool::Codex, "config.toml", src.as_bytes()).unwrap())
                .unwrap();
        assert!(
            !out.contains("projects") && !out.contains("trust_level"),
            "{out}"
        );
        assert!(out.contains("o3") && out.contains("npx"), "{out}");
        // Nothing to strip: bytes kept verbatim (comments and all).
        assert!(staged_bytes(Tool::Codex, "config.toml", b"# hi\nmodel = \"o3\"\n").is_none());
        assert!(staged_bytes(Tool::Copilot, "config.toml", src.as_bytes()).is_none());
    }

    #[test]
    fn toml_merge_keeps_local_keys_and_unions_arrays() {
        let tmp = tempfile::tempdir().unwrap();
        let (inc, ex) = (tmp.path().join("in.toml"), tmp.path().join("ex.toml"));
        write(
            &inc,
            "model = \"gpt-5\"\nwritable_roots = [\"/a\"]\n[mcp_servers.x]\ncommand = \"new\"\n",
        );
        write(&ex, "model = \"o3\"\nwritable_roots = [\"/b\"]\n[projects.\"/p\"]\ntrust_level = \"trusted\"\n[mcp_servers.x]\ncommand = \"old\"\nargs = [\"-v\"]\n");
        merge_toml_file(&inc, &ex).unwrap();
        let got: toml::Table = fs::read_to_string(&ex).unwrap().parse().unwrap();
        assert_eq!(got["model"].as_str(), Some("gpt-5"));
        assert_eq!(got["writable_roots"].as_array().unwrap().len(), 2);
        assert_eq!(
            got["projects"]["/p"]["trust_level"].as_str(),
            Some("trusted")
        );
        assert_eq!(got["mcp_servers"]["x"]["command"].as_str(), Some("new"));
        assert_eq!(got["mcp_servers"]["x"]["args"][0].as_str(), Some("-v"));
    }

    #[test]
    fn codex_executables_cover_notify_mcp_policy_hooks_and_rules() {
        let tmp = tempfile::tempdir().unwrap();
        let (src, home) = (tmp.path().join("src"), tmp.path().join("home"));
        write(
            &src.join("config.toml"),
            "notify = [\"python3\", \"/n.py\"]\napproval_policy = \"never\"\n\
             [mcp_servers.kept]\ncommand = \"kept\"\n[mcp_servers.evil]\ncommand = \"curl\"\nargs = [\"x\"]\n\
             [profiles.yolo]\nsandbox_mode = \"danger-full-access\"\n",
        );
        write(
            &home.join("config.toml"),
            "[mcp_servers.kept]\ncommand = \"kept\"\n",
        );
        write(
            &src.join("hooks.json"),
            r#"{"hooks":{"PreToolUse":[{"hooks":[{"type":"command","command":"./pre.sh"}]}]}}"#,
        );
        write(
            &src.join("rules/default.rules"),
            "# c\nprefix_rule(pattern=[\"git\"], decision=\"allow\")\nold\n",
        );
        write(&home.join("rules/default.rules"), "old\n");

        let got = new_executables(Tool::Codex, &src, &home, false).unwrap();
        let want: BTreeSet<String> = [
            "codex config.toml: approval_policy = \"never\"",
            "codex config.toml: curl x",
            "codex config.toml: notify = python3 /n.py",
            "codex config.toml: profiles.yolo.sandbox_mode = \"danger-full-access\"",
            "codex hooks.json: ./pre.sh",
            "codex rules/default.rules: prefix_rule(pattern=[\"git\"], decision=\"allow\")",
        ]
        .into_iter()
        .map(String::from)
        .collect();
        // The locally configured `kept` server and the existing rule line
        // don't re-prompt.
        assert_eq!(got, want);
    }

    #[test]
    fn copilot_executables_cover_hooks_mcp_lsp_and_scripts() {
        let tmp = tempfile::tempdir().unwrap();
        let (src, home) = (tmp.path().join("src"), tmp.path().join("home"));
        write(
            &src.join("settings.json"),
            r#"{"model":"x","hooks":{"preToolUse":[{"type":"command","bash":"./guard.sh"}]}}"#,
        );
        write(
            &src.join("mcp-config.json"),
            r#"{"mcpServers":{"gh":{"type":"local","command":"npx","args":["-y","srv"]},"web":{"type":"http","url":"https://x"}}}"#,
        );
        write(
            &src.join("lsp-config.json"),
            r#"{"lspServers":{"rust":{"command":"rust-analyzer"}}}"#,
        );
        write(&src.join("hooks/guard.sh"), "#!/bin/sh\nexit 0\n");
        write(&src.join("hooks/same.sh"), "same");
        write(&home.join("hooks/same.sh"), "same");

        let got = new_executables(Tool::Copilot, &src, &home, false).unwrap();
        for w in [
            "copilot settings.json: bash: ./guard.sh",
            "copilot mcp-config.json: npx -y srv",
            "copilot lsp-config.json: rust-analyzer",
            "copilot hooks/guard.sh: new or changed hook file",
        ] {
            assert!(got.contains(w), "missing {w}: {got:?}");
        }
        assert_eq!(got.len(), 4, "{got:?}");
    }

    #[test]
    fn backup_copies_only_overwritten_files() {
        let tmp = tempfile::tempdir().unwrap();
        let (src, home) = (tmp.path().join("src"), tmp.path().join(".codex"));
        write(&src.join("AGENTS.md"), "new");
        write(&src.join("prompts/p.md"), "added");
        write(&home.join("AGENTS.md"), "old");
        let backup = backup_overwrites(&src, &home).unwrap().unwrap();
        assert_eq!(fs::read_to_string(backup.join("AGENTS.md")).unwrap(), "old");
        assert!(!backup.join("prompts").exists());
        // Nothing to overwrite: no backup dir at all.
        assert!(backup_overwrites(&src, &tmp.path().join("empty"))
            .unwrap()
            .is_none());
    }
}
