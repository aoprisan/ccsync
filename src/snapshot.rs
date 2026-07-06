//! Building a snapshot: walk each enabled tool's data directory, apply its
//! include/exclude rules, hard-block credentials, optionally scan text configs
//! for secrets, copy the surviving files into the staging `data/<tool>/`
//! subtree, and write a manifest.
//!
//! Staging layout (manifest v2):
//! ```text
//! <staging>/
//! ├── manifest.json
//! └── data/
//!     ├── claude/          # mirrors the relevant subtree of ~/.claude
//!     │   ├── settings.json
//!     │   ├── CLAUDE.md
//!     │   └── projects/-home-user-x/session.jsonl
//!     └── copilot/         # mirrors the relevant subtree of ~/.copilot
//!         ├── settings.json
//!         └── session-state/<id>/events.jsonl
//! ```

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use sha2::{Digest, Sha256};
use walkdir::WalkDir;

use crate::config::Config;
use crate::error::CcError;
use crate::manifest::{FileEntry, Manifest, ProjectRoot, ToolEntry};
use crate::mcp;
use crate::paths;
use crate::redact;
use crate::tools::{self, ToolId, ToolPlan};

pub struct SnapshotOptions {
    pub dry_run: bool,
    pub allow_secrets: bool,
    /// `~/.claude.json` to harvest MCP server definitions from, when
    /// `config.include_mcp_servers` is set. `None` skips MCP bundling entirely.
    pub claude_json: Option<PathBuf>,
}

impl SnapshotOptions {
    /// Options for a real run, resolving the MCP source file from `config`
    /// (honoring `CLAUDE_CONFIG_DIR`). MCP bundling is skipped when the config
    /// disables it or the source file cannot be located.
    pub fn new(dry_run: bool, allow_secrets: bool, config: &Config) -> Self {
        let claude_json = if config.include_mcp_servers {
            paths::claude_json_file().ok()
        } else {
            None
        };
        SnapshotOptions {
            dry_run,
            allow_secrets,
            claude_json,
        }
    }
}

/// File extensions we treat as text and therefore scan for secrets.
const SCANNED_EXTS: &[&str] = &["json", "toml", "md", "yaml", "yml", "env"];

/// Reports copy progress while a snapshot is built. Implemented by the CLI to
/// drive a progress bar; `snapshot::build` itself stays UI-agnostic. Totals
/// aggregate across all tools in the run.
pub trait ProgressSink {
    /// Called once after the full file list is known, before any copying.
    fn start(&self, total_files: u64, total_bytes: u64);
    /// Called after each file is captured, with that file's byte count.
    fn advance(&self, file_bytes: u64);
    /// Called once when capture finishes.
    fn finish(&self);
}

/// A file selected for capture, resolved during the planning pass so progress
/// totals are known before any bytes are read.
struct PlannedFile {
    tool: ToolId,
    abs: PathBuf,
    rel: String,
    size: u64,
}

/// Build a snapshot of every tool in `plans` into `staging`. Returns the
/// manifest. A plan whose root directory does not exist contributes nothing.
pub fn build(
    plans: &[ToolPlan],
    staging: &Path,
    config: &Config,
    opts: &SnapshotOptions,
) -> Result<Manifest> {
    build_inner(plans, staging, config, opts, None)
}

/// Like [`build`], but reports per-file progress through `progress`.
pub fn build_with_progress(
    plans: &[ToolPlan],
    staging: &Path,
    config: &Config,
    opts: &SnapshotOptions,
    progress: &dyn ProgressSink,
) -> Result<Manifest> {
    build_inner(plans, staging, config, opts, Some(progress))
}

fn build_inner(
    plans: &[ToolPlan],
    staging: &Path,
    config: &Config,
    opts: &SnapshotOptions,
    progress: Option<&dyn ProgressSink>,
) -> Result<Manifest> {
    let host = hostname();
    let home = paths::home_dir()?.to_string_lossy().to_string();
    let mut manifest = Manifest::new(host, home);

    let data_root = staging.join("data");
    if !opts.dry_run {
        // Start from a clean staging data dir so removed files don't linger.
        if data_root.exists() {
            fs::remove_dir_all(&data_root)
                .with_context(|| format!("clearing staging dir {}", data_root.display()))?;
        }
        fs::create_dir_all(&data_root)?;
    }

    // Plan first: resolve the complete file list across all tools (applying
    // include/exclude and the credential hard-block) so progress has an
    // accurate total before any bytes are read or copied.
    let mut planned = Vec::new();
    for plan in plans {
        if !plan.root.exists() {
            continue;
        }
        manifest.tools.push(ToolEntry {
            tool: plan.id,
            source_root: plan.root.to_string_lossy().to_string(),
        });
        let spec = tools::spec(plan.id);
        for entry in &plan.include {
            if spec.session_entries.contains(&entry.as_str()) && !plan.include_sessions {
                continue;
            }
            let src = plan.root.join(entry);
            if !src.exists() {
                continue;
            }
            plan_path(&src, plan, &mut planned)?;
        }
    }

    let total_bytes: u64 = planned.iter().map(|p| p.size).sum();
    if let Some(p) = progress {
        p.start(planned.len() as u64, total_bytes);
    }
    for pf in &planned {
        capture_file(pf, &data_root, opts, &mut manifest)?;
        if let Some(p) = progress {
            p.advance(pf.size);
        }
    }
    if let Some(p) = progress {
        p.finish();
    }

    // Record decoded Claude project roots for remapping, even in dry-run.
    // (Copilot needs no equivalent: its remap is pure prefix rewriting.)
    if let Some(claude) = plans.iter().find(|p| p.id == ToolId::Claude) {
        if claude.include_sessions {
            let projects = claude.root.join("projects");
            if projects.is_dir() {
                for child in fs::read_dir(&projects)? {
                    let child = child?;
                    if child.file_type()?.is_dir() {
                        let encoded = child.file_name().to_string_lossy().to_string();
                        manifest.project_roots.push(ProjectRoot {
                            decoded_path: paths::decode_path(&encoded)
                                .to_string_lossy()
                                .to_string(),
                            encoded,
                        });
                    }
                }
            }
        }
    }

    // Bundle locally-configured MCP servers from `~/.claude.json` (outside the
    // captured `~/.claude` tree) into a standalone file in the Claude subtree.
    // Copilot needs no equivalent: its user-level `mcp-config.json` lives
    // inside `~/.copilot` and is captured as a plain include.
    if config.include_mcp_servers && plans.iter().any(|p| p.id == ToolId::Claude) {
        if let Some(claude_json) = &opts.claude_json {
            capture_mcp_servers(claude_json, &data_root, opts, &mut manifest)?;
        }
    }

    if !opts.dry_run {
        manifest.write_to(staging)?;
    }
    Ok(manifest)
}

/// Extract MCP server definitions from `claude_json` and stage them as
/// `mcp-servers.json`. The serialized blob is secret-scanned like any other
/// config (a server `env` carrying an API key aborts unless `--allow-secrets`).
fn capture_mcp_servers(
    claude_json: &Path,
    data_root: &Path,
    opts: &SnapshotOptions,
    manifest: &mut Manifest,
) -> Result<()> {
    let Some(doc) = mcp::extract(claude_json)? else {
        return Ok(());
    };
    let serialized = serde_json::to_string_pretty(&doc)?;

    if !opts.allow_secrets {
        if let Some(hint) = redact::scan_for_secrets(&serialized) {
            return Err(CcError::SecretDetected {
                file: mcp::MCP_FILE.to_string(),
                hint,
            }
            .into());
        }
    }

    let bytes = serialized.into_bytes();
    manifest.files.push(FileEntry {
        rel_path: mcp::MCP_FILE.to_string(),
        sha256: hex(&Sha256::digest(&bytes)),
        size: bytes.len() as u64,
        tool: ToolId::Claude,
    });

    if !opts.dry_run {
        let dest = data_root.join(ToolId::Claude.as_str()).join(mcp::MCP_FILE);
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(&dest, &bytes).with_context(|| format!("writing {}", dest.display()))?;
    }
    Ok(())
}

/// Walk a single include entry (file or directory tree) and append the files
/// that survive include/exclude to `out`. The credential hard-block aborts the
/// whole snapshot here, before any bytes are read.
fn plan_path(src: &Path, plan: &ToolPlan, out: &mut Vec<PlannedFile>) -> Result<()> {
    let spec = tools::spec(plan.id);
    for entry in WalkDir::new(src).follow_links(false) {
        let entry = entry?;
        if !entry.file_type().is_file() {
            continue;
        }
        let abs = entry.path();
        let rel = abs
            .strip_prefix(&plan.root)
            .expect("walked path is under the tool root")
            .to_string_lossy()
            .replace('\\', "/");

        // Hard block: credentials never leave the machine.
        if let Some(blocked) = redact::credential_block_match(spec, &rel) {
            return Err(CcError::CredentialBlocked(format!(
                "{} ({blocked})",
                format_rel(plan.id, &rel)
            ))
            .into());
        }

        if plan.is_excluded(&rel) {
            continue;
        }

        let size = entry.metadata().map(|m| m.len()).unwrap_or(0);
        out.push(PlannedFile {
            tool: plan.id,
            abs: abs.to_path_buf(),
            rel,
            size,
        });
    }
    Ok(())
}

/// `<tool>/<rel>` for error messages and dry-run listings.
fn format_rel(tool: ToolId, rel: &str) -> String {
    format!("{}/{rel}", tool.as_str())
}

/// Scan, hash, and (unless dry-run) copy a single planned file into staging.
fn capture_file(
    pf: &PlannedFile,
    data_root: &Path,
    opts: &SnapshotOptions,
    manifest: &mut Manifest,
) -> Result<()> {
    let abs = pf.abs.as_path();
    let rel = &pf.rel;

    // Secret scan for text configs unless explicitly allowed.
    if !opts.allow_secrets && is_scanned(abs) {
        if let Ok(text) = fs::read_to_string(abs) {
            if let Some(hint) = redact::scan_for_secrets(&text) {
                return Err(CcError::SecretDetected {
                    file: format_rel(pf.tool, rel),
                    hint,
                }
                .into());
            }
        }
    }

    let bytes = fs::read(abs).with_context(|| format!("reading {}", abs.display()))?;
    let sha256 = hex(&Sha256::digest(&bytes));
    manifest.files.push(FileEntry {
        rel_path: rel.clone(),
        sha256,
        size: bytes.len() as u64,
        tool: pf.tool,
    });

    if !opts.dry_run {
        let dest = data_root.join(pf.tool.as_str()).join(rel);
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(&dest, &bytes).with_context(|| format!("writing {}", dest.display()))?;
    }
    Ok(())
}

fn is_scanned(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| SCANNED_EXTS.contains(&e.to_ascii_lowercase().as_str()))
        .unwrap_or(false)
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(s, "{b:02x}");
    }
    s
}

fn hostname() -> String {
    std::env::var("HOSTNAME")
        .or_else(|_| std::env::var("COMPUTERNAME"))
        .unwrap_or_else(|_| "unknown".to_string())
}

/// Convenience used by `restore`/`pull` to confirm a staged snapshot exists.
pub fn require_staged(staging: &Path) -> Result<PathBuf> {
    let data = staging.join("data");
    if !staging.join(crate::manifest::MANIFEST_NAME).exists() || !data.exists() {
        return Err(CcError::NoStagedSnapshot(staging.display().to_string()).into());
    }
    Ok(data)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(path: &Path, content: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
    }

    fn claude_plan(root: &Path, cfg: &Config) -> ToolPlan {
        ToolPlan::for_tool(ToolId::Claude, root.to_path_buf(), cfg)
    }

    fn copilot_plan(root: &Path, cfg: &Config) -> ToolPlan {
        ToolPlan::for_tool(ToolId::Copilot, root.to_path_buf(), cfg)
    }

    #[test]
    fn captures_includes_and_skips_excludes() {
        let tmp = tempfile::tempdir().unwrap();
        let claude = tmp.path().join("claude");
        let staging = tmp.path().join("staging");
        write(&claude.join("settings.json"), r#"{"theme":"dark"}"#);
        write(&claude.join("CLAUDE.md"), "# memory");
        write(&claude.join("shell-snapshots/snap.sh"), "echo hi");
        write(
            &claude.join("projects/-home-alice-proj/sess.jsonl"),
            "{\"cwd\":\"/home/alice/proj\"}\n",
        );

        let cfg = Config::default();
        let opts = SnapshotOptions {
            dry_run: false,
            allow_secrets: false,
            claude_json: None,
        };
        let m = build(&[claude_plan(&claude, &cfg)], &staging, &cfg, &opts).unwrap();

        let rels: Vec<&str> = m.files.iter().map(|f| f.rel_path.as_str()).collect();
        assert!(rels.contains(&"settings.json"));
        assert!(rels.contains(&"CLAUDE.md"));
        assert!(rels.contains(&"projects/-home-alice-proj/sess.jsonl"));
        // Excluded machine-local state is not captured.
        assert!(!rels.iter().any(|r| r.starts_with("shell-snapshots")));
        // Project roots recorded for remapping.
        assert_eq!(m.project_roots.len(), 1);
        assert_eq!(m.project_roots[0].decoded_path, "/home/alice/proj");
        // Files land in the per-tool staging subtree; entries are tagged.
        assert!(staging.join("data/claude/settings.json").exists());
        assert!(m.files.iter().all(|f| f.tool == ToolId::Claude));
        assert_eq!(m.tools.len(), 1);
        assert_eq!(m.tools[0].tool, ToolId::Claude);
    }

    #[test]
    fn captures_both_tools_into_separate_subtrees() {
        let tmp = tempfile::tempdir().unwrap();
        let claude = tmp.path().join("claude");
        let copilot = tmp.path().join("copilot");
        let staging = tmp.path().join("staging");
        write(&claude.join("settings.json"), r#"{"theme":"dark"}"#);
        write(&copilot.join("settings.json"), r#"{"banner":"never"}"#);
        write(&copilot.join("mcp-config.json"), r#"{"mcpServers":{}}"#);
        write(
            &copilot.join("session-state/abc123/events.jsonl"),
            "{\"cwd\":\"/home/alice/proj\"}\n",
        );
        write(&copilot.join("logs/process-1-2.log"), "noise");
        write(&copilot.join("session-store.db"), "sqlite");

        let cfg = Config::default();
        let opts = SnapshotOptions {
            dry_run: false,
            allow_secrets: false,
            claude_json: None,
        };
        let plans = [claude_plan(&claude, &cfg), copilot_plan(&copilot, &cfg)];
        let m = build(&plans, &staging, &cfg, &opts).unwrap();

        assert!(staging.join("data/claude/settings.json").exists());
        assert!(staging.join("data/copilot/settings.json").exists());
        assert!(staging
            .join("data/copilot/session-state/abc123/events.jsonl")
            .exists());
        // Copilot noise is excluded.
        let copilot_rels: Vec<&str> = m
            .files
            .iter()
            .filter(|f| f.tool == ToolId::Copilot)
            .map(|f| f.rel_path.as_str())
            .collect();
        assert!(!copilot_rels.iter().any(|r| r.starts_with("logs")));
        assert!(!copilot_rels.contains(&"session-store.db"));
        // Both tools recorded with their source roots.
        assert_eq!(m.tools.len(), 2);
        assert_eq!(m.tools_present(), vec![ToolId::Claude, ToolId::Copilot]);
    }

    #[test]
    fn missing_tool_root_is_skipped_silently() {
        let tmp = tempfile::tempdir().unwrap();
        let claude = tmp.path().join("claude");
        let staging = tmp.path().join("staging");
        write(&claude.join("settings.json"), "{}");

        let cfg = Config::default();
        let opts = SnapshotOptions {
            dry_run: false,
            allow_secrets: false,
            claude_json: None,
        };
        let plans = [
            claude_plan(&claude, &cfg),
            copilot_plan(&tmp.path().join("no-such-copilot"), &cfg),
        ];
        let m = build(&plans, &staging, &cfg, &opts).unwrap();
        assert_eq!(m.tools_present(), vec![ToolId::Claude]);
    }

    #[test]
    fn hard_blocks_credentials() {
        let tmp = tempfile::tempdir().unwrap();
        let claude = tmp.path().join("claude");
        let staging = tmp.path().join("staging");
        write(&claude.join("settings.json"), "{}");
        write(&claude.join(".credentials.json"), r#"{"token":"x"}"#);
        // Add .credentials.json to includes to prove the hard block wins.
        let mut cfg = Config::default();
        cfg.include.push(".credentials.json".into());
        cfg.exclude.clear();
        let opts = SnapshotOptions {
            dry_run: false,
            allow_secrets: true,
            claude_json: None,
        };
        let err = build(&[claude_plan(&claude, &cfg)], &staging, &cfg, &opts).unwrap_err();
        assert!(err.to_string().contains("credential"));
    }

    #[test]
    fn hard_blocks_copilot_root_config_json() {
        let tmp = tempfile::tempdir().unwrap();
        let copilot = tmp.path().join("copilot");
        let staging = tmp.path().join("staging");
        write(&copilot.join("config.json"), r#"{"loggedInUsers":["x"]}"#);
        // Force-include it and clear excludes to prove the hard block wins
        // over configuration.
        let mut cfg = Config::default();
        cfg.copilot.include = vec!["config.json".into()];
        cfg.copilot.exclude.clear();
        let opts = SnapshotOptions {
            dry_run: false,
            allow_secrets: true,
            claude_json: None,
        };
        let err = build(&[copilot_plan(&copilot, &cfg)], &staging, &cfg, &opts).unwrap_err();
        assert!(err.to_string().contains("credential"));
    }

    #[test]
    fn nested_copilot_config_json_is_not_blocked() {
        let tmp = tempfile::tempdir().unwrap();
        let copilot = tmp.path().join("copilot");
        let staging = tmp.path().join("staging");
        write(
            &copilot.join("skills/my-skill/config.json"),
            r#"{"option":true}"#,
        );

        let cfg = Config::default();
        let opts = SnapshotOptions {
            dry_run: false,
            allow_secrets: false,
            claude_json: None,
        };
        let m = build(&[copilot_plan(&copilot, &cfg)], &staging, &cfg, &opts).unwrap();
        assert!(m
            .files
            .iter()
            .any(|f| f.rel_path == "skills/my-skill/config.json"));
    }

    #[test]
    fn hard_blocks_copilot_secret_dirs() {
        let tmp = tempfile::tempdir().unwrap();
        let copilot = tmp.path().join("copilot");
        let staging = tmp.path().join("staging");
        write(&copilot.join("mcp-secrets/index.json"), r#"{"k":"v"}"#);
        let mut cfg = Config::default();
        cfg.copilot.include = vec!["mcp-secrets".into()];
        cfg.copilot.exclude.clear();
        let opts = SnapshotOptions {
            dry_run: false,
            allow_secrets: true,
            claude_json: None,
        };
        let err = build(&[copilot_plan(&copilot, &cfg)], &staging, &cfg, &opts).unwrap_err();
        assert!(err.to_string().contains("credential"));
    }

    #[test]
    fn copilot_sessions_gated_by_include_sessions() {
        let tmp = tempfile::tempdir().unwrap();
        let copilot = tmp.path().join("copilot");
        let staging = tmp.path().join("staging");
        write(&copilot.join("settings.json"), "{}");
        write(
            &copilot.join("session-state/abc/events.jsonl"),
            "{\"x\":1}\n",
        );
        write(&copilot.join("command-history-state/history.json"), "{}");

        let mut cfg = Config::default();
        cfg.copilot.include_sessions = false;
        let opts = SnapshotOptions {
            dry_run: false,
            allow_secrets: false,
            claude_json: None,
        };
        let m = build(&[copilot_plan(&copilot, &cfg)], &staging, &cfg, &opts).unwrap();
        let rels: Vec<&str> = m.files.iter().map(|f| f.rel_path.as_str()).collect();
        assert_eq!(rels, vec!["settings.json"]);
    }

    #[test]
    fn aborts_on_secret_in_settings() {
        let tmp = tempfile::tempdir().unwrap();
        let claude = tmp.path().join("claude");
        let staging = tmp.path().join("staging");
        write(
            &claude.join("settings.json"),
            r#"{"env":{"ANTHROPIC_API_KEY":"sk-abcdefghijklmnopqrstuvwx"}}"#,
        );
        let cfg = Config::default();
        let opts = SnapshotOptions {
            dry_run: false,
            allow_secrets: false,
            claude_json: None,
        };
        let err = build(&[claude_plan(&claude, &cfg)], &staging, &cfg, &opts).unwrap_err();
        assert!(err.to_string().contains("secret"));

        // With allow_secrets it succeeds.
        let opts = SnapshotOptions {
            dry_run: false,
            allow_secrets: true,
            claude_json: None,
        };
        assert!(build(&[claude_plan(&claude, &cfg)], &staging, &cfg, &opts).is_ok());
    }

    #[test]
    fn bundles_mcp_servers_from_claude_json() {
        let tmp = tempfile::tempdir().unwrap();
        let claude = tmp.path().join("claude");
        let staging = tmp.path().join("staging");
        write(&claude.join("settings.json"), "{}");
        let claude_json = tmp.path().join(".claude.json");
        write(
            &claude_json,
            r#"{"oauthAccount":{"accessToken":"keep-local"},"mcpServers":{"fetch":{"command":"uvx"}}}"#,
        );

        let cfg = Config::default();
        let opts = SnapshotOptions {
            dry_run: false,
            allow_secrets: false,
            claude_json: Some(claude_json),
        };
        let m = build(&[claude_plan(&claude, &cfg)], &staging, &cfg, &opts).unwrap();

        assert!(m.files.iter().any(|f| f.rel_path == crate::mcp::MCP_FILE));
        let staged = staging.join("data/claude").join(crate::mcp::MCP_FILE);
        let doc: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&staged).unwrap()).unwrap();
        assert_eq!(doc["mcpServers"]["fetch"]["command"], "uvx");
        // The OAuth token from ~/.claude.json never enters the snapshot.
        assert!(doc.get("oauthAccount").is_none());
    }

    #[test]
    fn reports_progress_totals() {
        use std::cell::Cell;

        struct CountingSink {
            files: Cell<u64>,
            bytes: Cell<u64>,
            advanced: Cell<u64>,
            finished: Cell<bool>,
        }
        impl ProgressSink for CountingSink {
            fn start(&self, total_files: u64, total_bytes: u64) {
                self.files.set(total_files);
                self.bytes.set(total_bytes);
            }
            fn advance(&self, file_bytes: u64) {
                self.advanced.set(self.advanced.get() + file_bytes);
            }
            fn finish(&self) {
                self.finished.set(true);
            }
        }

        let tmp = tempfile::tempdir().unwrap();
        let claude = tmp.path().join("claude");
        let copilot = tmp.path().join("copilot");
        let staging = tmp.path().join("staging");
        write(&claude.join("settings.json"), r#"{"theme":"dark"}"#);
        write(&claude.join("CLAUDE.md"), "# memory");
        write(&copilot.join("settings.json"), r#"{"a":1}"#);

        let cfg = Config::default();
        let opts = SnapshotOptions {
            dry_run: false,
            allow_secrets: false,
            claude_json: None,
        };
        let sink = CountingSink {
            files: Cell::new(0),
            bytes: Cell::new(0),
            advanced: Cell::new(0),
            finished: Cell::new(false),
        };
        let plans = [claude_plan(&claude, &cfg), copilot_plan(&copilot, &cfg)];
        let m = build_with_progress(&plans, &staging, &cfg, &opts, &sink).unwrap();

        // Totals aggregate across both tools.
        let total: u64 = m.files.iter().map(|f| f.size).sum();
        assert_eq!(sink.files.get(), m.files.len() as u64);
        assert_eq!(sink.files.get(), 3);
        assert_eq!(sink.bytes.get(), total);
        assert_eq!(sink.advanced.get(), total);
        assert!(sink.finished.get());
    }

    #[test]
    fn mcp_bundling_disabled_by_config() {
        let tmp = tempfile::tempdir().unwrap();
        let claude = tmp.path().join("claude");
        let staging = tmp.path().join("staging");
        write(&claude.join("settings.json"), "{}");
        let claude_json = tmp.path().join(".claude.json");
        write(
            &claude_json,
            r#"{"mcpServers":{"fetch":{"command":"uvx"}}}"#,
        );

        let cfg = Config {
            include_mcp_servers: false,
            ..Config::default()
        };
        let opts = SnapshotOptions {
            dry_run: false,
            allow_secrets: false,
            claude_json: Some(claude_json),
        };
        let m = build(&[claude_plan(&claude, &cfg)], &staging, &cfg, &opts).unwrap();
        assert!(!m.files.iter().any(|f| f.rel_path == crate::mcp::MCP_FILE));
    }
}
