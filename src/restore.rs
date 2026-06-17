//! Restoring a staged snapshot onto the local machine. Before touching
//! `~/.claude`, the existing directory is backed up to a timestamped sibling so
//! a restore is always reversible. Path remapping is applied to the staged
//! `data/` first (unless disabled), then files are copied in — either
//! overwriting or, for `settings.json`, deep-merging with the local file.

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
}

pub struct RestoreReport {
    pub backup_dir: Option<PathBuf>,
    pub files_written: Vec<String>,
    pub mappings: Vec<remap::Mapping>,
    /// Number of MCP server definitions merged into `~/.claude.json`.
    pub mcp_servers_restored: usize,
    /// Backup copy of `~/.claude.json` taken before merging MCP servers in.
    pub claude_json_backup: Option<PathBuf>,
}

/// Apply the staged snapshot to `claude_dir`.
pub fn run(
    claude_dir: &Path,
    staging: &Path,
    config: &Config,
    opts: &RestoreOptions,
) -> Result<RestoreReport> {
    let data_root = snapshot::require_staged(staging)?;
    let manifest = Manifest::read_from(staging)?;

    // Compute path mappings.
    let local_home = paths::home_dir()?.to_string_lossy().to_string();
    let mappings = if opts.remap {
        remap::build_mappings(&manifest, &local_home, &config.remap)
    } else {
        Vec::new()
    };

    // Back up the existing claude dir.
    let backup_dir = if !opts.dry_run && claude_dir.exists() {
        let ts = chrono::Utc::now().format("%Y%m%d-%H%M%S");
        let backup = claude_dir.with_file_name(format!(
            "{}.ccsync-backup-{ts}",
            claude_dir
                .file_name()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_else(|| ".claude".to_string())
        ));
        copy_dir(claude_dir, &backup).with_context(|| {
            format!(
                "backing up {} to {}",
                claude_dir.display(),
                backup.display()
            )
        })?;
        Some(backup)
    } else {
        None
    };

    // Apply remapping to the staged data in place (skipped for dry-run since it
    // mutates staging; mappings are still reported).
    if !opts.dry_run && opts.remap {
        remap::apply(&data_root, &mappings)?;
    }

    // Copy staged files into the claude dir.
    let mut files_written = Vec::new();
    for entry in WalkDir::new(&data_root) {
        let entry = entry?;
        if !entry.file_type().is_file() {
            continue;
        }
        let rel = entry.path().strip_prefix(&data_root).unwrap();
        let rel_str = rel.to_string_lossy().replace('\\', "/");

        // The bundled MCP servers file is not a `~/.claude` file; it is merged
        // into `~/.claude.json` separately below, not copied into the dir.
        if rel_str == mcp::MCP_FILE {
            continue;
        }

        let dest = claude_dir.join(rel);
        files_written.push(rel_str.clone());

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
            fs::copy(entry.path(), &dest).with_context(|| format!("writing {}", dest.display()))?;
        }
    }

    // Merge bundled MCP servers into the local `~/.claude.json`, remapping
    // per-project paths exactly as session directories were remapped above.
    let mut mcp_servers_restored = 0;
    let mut claude_json_backup = None;
    let mcp_staged = data_root.join(mcp::MCP_FILE);
    if mcp_staged.exists() {
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
        backup_dir,
        files_written,
        mappings,
        mcp_servers_restored,
        claude_json_backup,
    })
}

/// Deep-merge the JSON in `incoming` into the JSON at `existing`, writing the
/// merged result back to `existing`. Objects merge key-by-key; arrays and
/// scalars from `incoming` win.
fn merge_json_file(incoming: &Path, existing: &Path) -> Result<()> {
    let inc: serde_json::Value = serde_json::from_str(&fs::read_to_string(incoming)?)
        .with_context(|| format!("parsing {}", incoming.display()))?;
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
    use crate::snapshot::SnapshotOptions;

    fn write(path: &Path, content: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
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
            &src_claude,
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
        };
        let report = run(&dst_claude, &staging, &cfg, &opts).unwrap();

        // Backup happened.
        assert!(report.backup_dir.is_some());
        assert!(report.backup_dir.unwrap().join("old.txt").exists());

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
    fn merge_mode_deep_merges_json() {
        let tmp = tempfile::tempdir().unwrap();
        let staging = tmp.path().join("staging");
        let data = staging.join("data");
        write(
            &data.join("settings.json"),
            r#"{"model":"opus","env":{"A":"1"}}"#,
        );
        // Minimal manifest so require_staged/read pass.
        let m = Manifest::new(
            "h".into(),
            paths::home_dir().unwrap().to_string_lossy().to_string(),
        );
        m.write_to(&staging).unwrap();

        let claude = tmp.path().join("claude");
        write(
            &claude.join("settings.json"),
            r#"{"theme":"dark","env":{"B":"2"}}"#,
        );

        let opts = RestoreOptions {
            dry_run: false,
            remap: false,
            merge: MergeMode::Merge,
            claude_json: None,
        };
        run(&claude, &staging, &Config::default(), &opts).unwrap();

        let merged: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(claude.join("settings.json")).unwrap())
                .unwrap();
        assert_eq!(merged["theme"], "dark");
        assert_eq!(merged["model"], "opus");
        assert_eq!(merged["env"]["A"], "1");
        assert_eq!(merged["env"]["B"], "2");
    }

    #[test]
    fn restores_mcp_servers_into_claude_json() {
        let tmp = tempfile::tempdir().unwrap();
        let staging = tmp.path().join("staging");
        let data = staging.join("data");
        write(&data.join("settings.json"), r#"{"theme":"dark"}"#);
        // A bundled MCP document riding in the snapshot.
        write(
            &data.join(crate::mcp::MCP_FILE),
            r#"{"mcpServers":{"fetch":{"command":"uvx"}}}"#,
        );
        let m = Manifest::new(
            "h".into(),
            paths::home_dir().unwrap().to_string_lossy().to_string(),
        );
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
            dry_run: false,
            remap: false,
            merge: MergeMode::Merge,
            claude_json: Some(claude_json.clone()),
        };
        let report = run(&claude, &staging, &Config::default(), &opts).unwrap();

        // The MCP file is not copied into ~/.claude.
        assert!(!claude.join(crate::mcp::MCP_FILE).exists());
        assert!(!report
            .files_written
            .iter()
            .any(|f| f == crate::mcp::MCP_FILE));
        // Server merged into ~/.claude.json; the OAuth token is preserved.
        assert_eq!(report.mcp_servers_restored, 1);
        assert!(report.claude_json_backup.is_some());
        let root: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&claude_json).unwrap()).unwrap();
        assert_eq!(root["mcpServers"]["fetch"]["command"], "uvx");
        assert_eq!(root["oauthAccount"]["accessToken"], "keep-me");
    }
}
