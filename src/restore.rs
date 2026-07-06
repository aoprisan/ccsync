//! Restoring a staged snapshot onto the local machine. For each tool the
//! snapshot carries, the tool's existing directory is backed up to a
//! timestamped sibling before anything is written, so a restore is always
//! reversible. Path remapping is applied to each tool's staged subtree first
//! (unless disabled), then files are copied in — either overwriting or
//! deep-merging JSON config files.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use walkdir::WalkDir;

use crate::config::Config;
use crate::manifest::Manifest;
use crate::mcp;
use crate::paths;
use crate::remap;
use crate::snapshot;
use crate::tools::{self, ToolId};

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum MergeMode {
    /// Replace files wholesale (still backs up first).
    Overwrite,
    /// Deep-merge JSON config files; overwrite everything else.
    Merge,
}

pub struct RestoreOptions {
    pub dry_run: bool,
    pub remap: bool,
    pub merge: MergeMode,
    /// Local `~/.claude.json` to merge bundled MCP servers into. `None` skips
    /// MCP restore (e.g. when MCP bundling is disabled or the path is unknown).
    pub claude_json: Option<PathBuf>,
    /// Restrict the restore to these tools. Empty restores every tool the
    /// snapshot carries.
    pub tools: Vec<ToolId>,
}

pub struct RestoreReport {
    /// One pre-restore backup per tool root that already existed.
    pub backups: Vec<(ToolId, PathBuf)>,
    /// Restored files as `<tool>/<rel>` for display.
    pub files_written: Vec<String>,
    pub mappings: Vec<remap::Mapping>,
    /// Number of MCP server definitions merged into `~/.claude.json`.
    pub mcp_servers_restored: usize,
    /// Backup copy of `~/.claude.json` taken before merging MCP servers in.
    pub claude_json_backup: Option<PathBuf>,
}

/// Apply the staged snapshot, routing each tool's files to its local data
/// directory (resolved through `paths::tool_dir`).
pub fn run(staging: &Path, config: &Config, opts: &RestoreOptions) -> Result<RestoreReport> {
    snapshot::require_staged(staging)?;
    let manifest = Manifest::read_from(staging)?;

    // Compute path mappings (shared across tools: the home-prefix rewrite is
    // tool-agnostic).
    let local_home = paths::home_dir()?.to_string_lossy().to_string();
    let mappings = if opts.remap {
        remap::build_mappings(&manifest, &local_home, &config.remap)
    } else {
        Vec::new()
    };

    let mut backups = Vec::new();
    let mut files_written = Vec::new();
    for tool in manifest.tools_present() {
        if !opts.tools.is_empty() && !opts.tools.contains(&tool) {
            continue;
        }
        let tool_data = manifest.tool_data_root(staging, tool);
        if !tool_data.is_dir() {
            continue;
        }
        let target = paths::tool_dir(tool)?;

        // Back up the existing tool dir.
        if !opts.dry_run && target.exists() {
            let backup = backup_root(&target)?;
            backups.push((tool, backup));
        }

        // Apply remapping to the staged data in place (skipped for dry-run
        // since it mutates staging; mappings are still reported).
        if !opts.dry_run && opts.remap {
            remap::apply(&tool_data, &mappings, &tools::spec(tool).remap)?;
        }

        // Copy staged files into the tool dir.
        for entry in WalkDir::new(&tool_data) {
            let entry = entry?;
            if !entry.file_type().is_file() {
                continue;
            }
            let rel = entry.path().strip_prefix(&tool_data).unwrap();
            let rel_str = rel.to_string_lossy().replace('\\', "/");

            // The bundled MCP servers file is not a `~/.claude` file; it is
            // merged into `~/.claude.json` separately below, not copied in.
            if tool == ToolId::Claude && rel_str == mcp::MCP_FILE {
                continue;
            }

            let dest = target.join(rel);
            files_written.push(format!("{}/{rel_str}", tool.as_str()));

            if opts.dry_run {
                continue;
            }
            if let Some(parent) = dest.parent() {
                fs::create_dir_all(parent)?;
            }
            let is_json_config =
                opts.merge == MergeMode::Merge && rel_str.ends_with(".json") && dest.exists();
            if is_json_config {
                merge_json_file(entry.path(), &dest)?;
            } else {
                fs::copy(entry.path(), &dest)
                    .with_context(|| format!("writing {}", dest.display()))?;
            }
        }
    }

    // Merge bundled MCP servers into the local `~/.claude.json`, remapping
    // per-project paths exactly as session directories were remapped above.
    let mut mcp_servers_restored = 0;
    let mut claude_json_backup = None;
    let restoring_claude = opts.tools.is_empty() || opts.tools.contains(&ToolId::Claude);
    let mcp_staged = manifest
        .tool_data_root(staging, ToolId::Claude)
        .join(mcp::MCP_FILE);
    if restoring_claude && mcp_staged.exists() {
        if let Some(claude_json) = &opts.claude_json {
            let doc: serde_json::Value = serde_json::from_str(&fs::read_to_string(&mcp_staged)?)
                .with_context(|| format!("parsing {}", mcp_staged.display()))?;
            mcp_servers_restored = mcp::server_count(&doc);
            if !opts.dry_run {
                // Back up the existing `~/.claude.json` first (it lives outside
                // `~/.claude`, so the directory backup above does not cover it).
                if claude_json.exists() {
                    let ts = chrono::Utc::now().format("%Y%m%d-%H%M%S");
                    let backup = claude_json.with_file_name(format!(
                        "{}.ccsync-backup-{ts}",
                        claude_json
                            .file_name()
                            .map(|s| s.to_string_lossy().to_string())
                            .unwrap_or_else(|| ".claude.json".to_string())
                    ));
                    fs::copy(claude_json, &backup).with_context(|| {
                        format!(
                            "backing up {} to {}",
                            claude_json.display(),
                            backup.display()
                        )
                    })?;
                    claude_json_backup = Some(backup);
                }
                let overwrite = opts.merge == MergeMode::Overwrite;
                mcp_servers_restored = mcp::merge_into(claude_json, &doc, &mappings, overwrite)?;
            }
        }
    }

    Ok(RestoreReport {
        backups,
        files_written,
        mappings,
        mcp_servers_restored,
        claude_json_backup,
    })
}

/// Copy `target` to a timestamped `<target>.ccsync-backup-<ts>` sibling and
/// return the backup path.
fn backup_root(target: &Path) -> Result<PathBuf> {
    let ts = chrono::Utc::now().format("%Y%m%d-%H%M%S");
    let backup = target.with_file_name(format!(
        "{}.ccsync-backup-{ts}",
        target
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| "tool".to_string())
    ));
    copy_dir(target, &backup)
        .with_context(|| format!("backing up {} to {}", target.display(), backup.display()))?;
    Ok(backup)
}

/// Deep-merge the JSON in `incoming` into the JSON at `existing`, writing the
/// merged result back to `existing`. Objects merge key-by-key; arrays and
/// scalars from `incoming` win. Content that does not parse as strict JSON
/// (Copilot's `settings.json` is JSONC, and either side may be malformed) is
/// restored as a plain overwrite copy instead of aborting the restore.
fn merge_json_file(incoming: &Path, existing: &Path) -> Result<()> {
    let incoming_text = fs::read_to_string(incoming)?;
    let Ok(inc) = serde_json::from_str::<serde_json::Value>(&incoming_text) else {
        fs::copy(incoming, existing).with_context(|| format!("writing {}", existing.display()))?;
        return Ok(());
    };
    let mut base: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(existing)?).unwrap_or(serde_json::Value::Null);
    merge_value(&mut base, inc);
    fs::write(existing, serde_json::to_string_pretty(&base)?)?;
    Ok(())
}

fn merge_value(base: &mut serde_json::Value, incoming: serde_json::Value) {
    match (base, incoming) {
        (serde_json::Value::Object(b), serde_json::Value::Object(i)) => {
            for (k, v) in i {
                merge_value(b.entry(k).or_insert(serde_json::Value::Null), v);
            }
        }
        (b, i) => *b = i,
    }
}

/// Recursively copy a directory tree.
fn copy_dir(src: &Path, dst: &Path) -> Result<()> {
    for entry in WalkDir::new(src) {
        let entry = entry?;
        let rel = entry.path().strip_prefix(src).unwrap();
        let target = dst.join(rel);
        if entry.file_type().is_dir() {
            fs::create_dir_all(&target)?;
        } else if entry.file_type().is_file() {
            if let Some(parent) = target.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::copy(entry.path(), &target)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::ToolEntry;
    use crate::snapshot::SnapshotOptions;
    use crate::tools::ToolPlan;

    fn write(path: &Path, content: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
    }

    fn opts_default() -> RestoreOptions {
        RestoreOptions {
            dry_run: false,
            remap: false,
            merge: MergeMode::Merge,
            claude_json: None,
            tools: Vec::new(),
        }
    }

    /// Point Claude's dir resolution at `dir` for the duration of `f`.
    fn with_claude_dir<T>(dir: &Path, f: impl FnOnce() -> T) -> T {
        let prev = std::env::var("CLAUDE_CONFIG_DIR").ok();
        std::env::set_var("CLAUDE_CONFIG_DIR", dir);
        let out = f();
        match prev {
            Some(p) => std::env::set_var("CLAUDE_CONFIG_DIR", p),
            None => std::env::remove_var("CLAUDE_CONFIG_DIR"),
        }
        out
    }

    /// A minimal v2 manifest that claims a Claude subtree, so tests can stage
    /// files by hand without running a snapshot.
    fn claude_manifest(source_home: &str) -> Manifest {
        let mut m = Manifest::new("h".into(), source_home.into());
        m.tools.push(ToolEntry {
            tool: ToolId::Claude,
            source_root: format!("{source_home}/.claude"),
        });
        m
    }

    #[test]
    fn restore_backs_up_remaps_and_writes() {
        let tmp = tempfile::tempdir().unwrap();
        // Source machine claude dir.
        let src_claude = tmp.path().join("src-claude");
        write(&src_claude.join("settings.json"), r#"{"theme":"dark"}"#);
        write(
            &src_claude.join("projects/-Users-alice-proj/s.jsonl"),
            "{\"cwd\":\"/Users/alice/proj\"}\n",
        );

        // Snapshot it, but forge the manifest's source_home so remap has work.
        let staging = tmp.path().join("staging");
        let cfg = Config::default();
        let mut manifest = snapshot::build(
            &[ToolPlan::for_tool(ToolId::Claude, src_claude.clone(), &cfg)],
            &staging,
            &cfg,
            &SnapshotOptions {
                dry_run: false,
                allow_secrets: false,
                claude_json: None,
            },
        )
        .unwrap();
        manifest.source_home = "/Users/alice".to_string();
        manifest.write_to(&staging).unwrap();

        // Target machine claude dir (pre-existing -> should be backed up).
        let dst_claude = tmp.path().join("dst-claude");
        write(&dst_claude.join("old.txt"), "existing");

        // Force local_home via HOME so remap maps /Users/alice -> here.
        let fake_home = tmp.path().join("home-bob");
        fs::create_dir_all(&fake_home).unwrap();
        std::env::set_var("HOME", &fake_home);

        let opts = RestoreOptions {
            dry_run: false,
            remap: true,
            merge: MergeMode::Overwrite,
            claude_json: None,
            tools: Vec::new(),
        };
        let report = with_claude_dir(&dst_claude, || run(&staging, &cfg, &opts)).unwrap();

        // Backup happened.
        assert_eq!(report.backups.len(), 1);
        assert_eq!(report.backups[0].0, ToolId::Claude);
        assert!(report.backups[0].1.join("old.txt").exists());

        // settings landed.
        assert!(dst_claude.join("settings.json").exists());

        // Project dir was remapped to the fake home and cwd rewritten.
        let new_home_str = fake_home.to_string_lossy().to_string();
        let encoded = paths::encode_path(Path::new(&format!("{new_home_str}/proj")));
        let restored_sess = dst_claude.join("projects").join(&encoded).join("s.jsonl");
        assert!(
            restored_sess.exists(),
            "expected remapped session dir {encoded}"
        );
        let content = fs::read_to_string(restored_sess).unwrap();
        assert!(content.contains(&new_home_str));
        assert!(!content.contains("/Users/alice"));
    }

    #[test]
    fn restores_both_tools_with_separate_backups() {
        let tmp = tempfile::tempdir().unwrap();
        let src_claude = tmp.path().join("src-claude");
        let src_copilot = tmp.path().join("src-copilot");
        write(&src_claude.join("settings.json"), r#"{"theme":"dark"}"#);
        write(&src_copilot.join("settings.json"), r#"{"banner":"never"}"#);
        write(
            &src_copilot.join("session-state/abc/events.jsonl"),
            "{\"cwd\":\"/Users/alice/proj\"}\n",
        );
        write(
            &src_copilot.join("permissions-config.json"),
            r#"{"/Users/alice/proj":{"allow":["shell"]}}"#,
        );

        let staging = tmp.path().join("staging");
        let cfg = Config::default();
        let mut manifest = snapshot::build(
            &[
                ToolPlan::for_tool(ToolId::Claude, src_claude, &cfg),
                ToolPlan::for_tool(ToolId::Copilot, src_copilot, &cfg),
            ],
            &staging,
            &cfg,
            &SnapshotOptions {
                dry_run: false,
                allow_secrets: false,
                claude_json: None,
            },
        )
        .unwrap();
        manifest.source_home = "/Users/alice".to_string();
        manifest.write_to(&staging).unwrap();

        // Pre-existing target dirs on the "new machine".
        let fake_home = tmp.path().join("home-bob");
        let dst_claude = fake_home.join(".claude");
        let dst_copilot = fake_home.join(".copilot");
        write(&dst_claude.join("old.txt"), "old-claude");
        write(&dst_copilot.join("old.txt"), "old-copilot");
        std::env::set_var("HOME", &fake_home);

        let prev_copilot = std::env::var("COPILOT_HOME").ok();
        std::env::set_var("COPILOT_HOME", &dst_copilot);
        let opts = RestoreOptions {
            dry_run: false,
            remap: true,
            merge: MergeMode::Overwrite,
            claude_json: None,
            tools: Vec::new(),
        };
        let report = with_claude_dir(&dst_claude, || run(&staging, &cfg, &opts)).unwrap();
        match prev_copilot {
            Some(p) => std::env::set_var("COPILOT_HOME", p),
            None => std::env::remove_var("COPILOT_HOME"),
        }

        // One timestamped backup per tool root.
        assert_eq!(report.backups.len(), 2);
        assert!(report.backups.iter().any(|(t, _)| *t == ToolId::Claude));
        assert!(report.backups.iter().any(|(t, _)| *t == ToolId::Copilot));

        // Routing: no cross-contamination.
        assert!(dst_claude.join("settings.json").exists());
        assert!(dst_copilot.join("settings.json").exists());
        assert!(!dst_claude.join("session-state").exists());

        // Copilot content remapped: session events and permission keys point
        // at the new home; the session dir name is untouched.
        let events =
            fs::read_to_string(dst_copilot.join("session-state/abc/events.jsonl")).unwrap();
        let new_home_str = fake_home.to_string_lossy().to_string();
        assert!(events.contains(&new_home_str));
        assert!(!events.contains("/Users/alice"));
        let perms = fs::read_to_string(dst_copilot.join("permissions-config.json")).unwrap();
        assert!(perms.contains(&format!("{new_home_str}/proj")));
        assert!(!perms.contains("/Users/alice"));
    }

    #[test]
    fn v1_flat_snapshot_restores_into_claude() {
        // A staged snapshot exactly as a pre-multi-tool ccsync would leave it:
        // flat data/ layout and a v1 manifest with no tool tags.
        let tmp = tempfile::tempdir().unwrap();
        let staging = tmp.path().join("staging");
        write(&staging.join("data/settings.json"), r#"{"theme":"dark"}"#);
        write(
            &staging.join("data/projects/-Users-alice-proj/s.jsonl"),
            "{\"cwd\":\"/Users/alice/proj\"}\n",
        );
        let v1 = r#"{
            "manifest_version": 1,
            "ccsync_version": "0.1.0",
            "source_host": "old-host",
            "source_home": "/Users/alice",
            "created_at": "2026-01-01T00:00:00Z",
            "files": [
                {"rel_path": "settings.json", "sha256": "x", "size": 16},
                {"rel_path": "projects/-Users-alice-proj/s.jsonl", "sha256": "y", "size": 30}
            ],
            "project_roots": [
                {"encoded": "-Users-alice-proj", "decoded_path": "/Users/alice/proj"}
            ]
        }"#;
        fs::write(staging.join(crate::manifest::MANIFEST_NAME), v1).unwrap();

        let fake_home = tmp.path().join("home-bob");
        fs::create_dir_all(&fake_home).unwrap();
        std::env::set_var("HOME", &fake_home);
        let dst_claude = tmp.path().join("dst-claude");

        let opts = RestoreOptions {
            dry_run: false,
            remap: true,
            merge: MergeMode::Overwrite,
            claude_json: None,
            tools: Vec::new(),
        };
        let report =
            with_claude_dir(&dst_claude, || run(&staging, &Config::default(), &opts)).unwrap();

        assert!(dst_claude.join("settings.json").exists());
        let new_home_str = fake_home.to_string_lossy().to_string();
        let encoded = paths::encode_path(Path::new(&format!("{new_home_str}/proj")));
        assert!(dst_claude
            .join("projects")
            .join(&encoded)
            .join("s.jsonl")
            .exists());
        // Nothing was misrouted into a "claude/" subdir of ~/.claude.
        assert!(!dst_claude.join("claude").exists());
        assert!(report
            .files_written
            .iter()
            .any(|f| f == "claude/settings.json"));
    }

    #[test]
    fn merge_mode_deep_merges_json() {
        let tmp = tempfile::tempdir().unwrap();
        let staging = tmp.path().join("staging");
        write(
            &staging.join("data/claude/settings.json"),
            r#"{"model":"opus","env":{"A":"1"}}"#,
        );
        let m = claude_manifest(&paths::home_dir().unwrap().to_string_lossy());
        m.write_to(&staging).unwrap();

        let claude = tmp.path().join("claude");
        write(
            &claude.join("settings.json"),
            r#"{"theme":"dark","env":{"B":"2"}}"#,
        );

        with_claude_dir(&claude, || {
            run(&staging, &Config::default(), &opts_default())
        })
        .unwrap();

        let merged: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(claude.join("settings.json")).unwrap())
                .unwrap();
        assert_eq!(merged["theme"], "dark");
        assert_eq!(merged["model"], "opus");
        assert_eq!(merged["env"]["A"], "1");
        assert_eq!(merged["env"]["B"], "2");
    }

    #[test]
    fn jsonc_settings_fall_back_to_overwrite_copy() {
        let tmp = tempfile::tempdir().unwrap();
        let staging = tmp.path().join("staging");
        let jsonc = "{\n  // banner is noisy\n  \"banner\": \"never\"\n}\n";
        write(&staging.join("data/copilot/settings.json"), jsonc);
        let mut m = claude_manifest(&paths::home_dir().unwrap().to_string_lossy());
        m.tools.push(ToolEntry {
            tool: ToolId::Copilot,
            source_root: "/x/.copilot".into(),
        });
        m.write_to(&staging).unwrap();

        let copilot = tmp.path().join("copilot");
        write(&copilot.join("settings.json"), r#"{"theme":"dark"}"#);

        let prev = std::env::var("COPILOT_HOME").ok();
        std::env::set_var("COPILOT_HOME", &copilot);
        let claude = tmp.path().join("claude");
        let result = with_claude_dir(&claude, || {
            run(&staging, &Config::default(), &opts_default())
        });
        match prev {
            Some(p) => std::env::set_var("COPILOT_HOME", p),
            None => std::env::remove_var("COPILOT_HOME"),
        }
        result.unwrap();

        // Merge mode couldn't parse the JSONC, so it copied it verbatim
        // instead of erroring out mid-restore.
        let restored = fs::read_to_string(copilot.join("settings.json")).unwrap();
        assert_eq!(restored, jsonc);
    }

    #[test]
    fn tool_filter_limits_restore() {
        let tmp = tempfile::tempdir().unwrap();
        let staging = tmp.path().join("staging");
        write(&staging.join("data/claude/settings.json"), "{}");
        write(&staging.join("data/copilot/settings.json"), "{}");
        let mut m = claude_manifest(&paths::home_dir().unwrap().to_string_lossy());
        m.tools.push(ToolEntry {
            tool: ToolId::Copilot,
            source_root: "/x/.copilot".into(),
        });
        m.write_to(&staging).unwrap();

        let claude = tmp.path().join("claude");
        let copilot = tmp.path().join("copilot");
        let prev = std::env::var("COPILOT_HOME").ok();
        std::env::set_var("COPILOT_HOME", &copilot);
        let opts = RestoreOptions {
            tools: vec![ToolId::Claude],
            ..opts_default()
        };
        let result = with_claude_dir(&claude, || run(&staging, &Config::default(), &opts));
        match prev {
            Some(p) => std::env::set_var("COPILOT_HOME", p),
            None => std::env::remove_var("COPILOT_HOME"),
        }
        result.unwrap();

        assert!(claude.join("settings.json").exists());
        assert!(!copilot.exists());
    }

    #[test]
    fn restores_mcp_servers_into_claude_json() {
        let tmp = tempfile::tempdir().unwrap();
        let staging = tmp.path().join("staging");
        write(
            &staging.join("data/claude/settings.json"),
            r#"{"theme":"dark"}"#,
        );
        // A bundled MCP document riding in the snapshot.
        write(
            &staging.join("data/claude").join(crate::mcp::MCP_FILE),
            r#"{"mcpServers":{"fetch":{"command":"uvx"}}}"#,
        );
        let m = claude_manifest(&paths::home_dir().unwrap().to_string_lossy());
        m.write_to(&staging).unwrap();

        let claude = tmp.path().join("claude");
        write(&claude.join("settings.json"), "{}");
        // Pre-existing ~/.claude.json with a token that must survive the merge.
        let claude_json = tmp.path().join(".claude.json");
        write(
            &claude_json,
            r#"{"oauthAccount":{"accessToken":"keep-me"}}"#,
        );

        let opts = RestoreOptions {
            claude_json: Some(claude_json.clone()),
            ..opts_default()
        };
        let report = with_claude_dir(&claude, || run(&staging, &Config::default(), &opts)).unwrap();

        // The MCP file is not copied into ~/.claude.
        assert!(!claude.join(crate::mcp::MCP_FILE).exists());
        assert!(!report
            .files_written
            .iter()
            .any(|f| f.ends_with(crate::mcp::MCP_FILE)));
        // Server merged into ~/.claude.json; the OAuth token is preserved.
        assert_eq!(report.mcp_servers_restored, 1);
        assert!(report.claude_json_backup.is_some());
        let root: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&claude_json).unwrap()).unwrap();
        assert_eq!(root["mcpServers"]["fetch"]["command"], "uvx");
        assert_eq!(root["oauthAccount"]["accessToken"], "keep-me");
    }
}
