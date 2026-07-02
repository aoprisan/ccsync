//! Building a snapshot: walk `~/.claude`, apply the include/exclude rules,
//! hard-block credentials, optionally scan text configs for secrets, copy the
//! surviving files into the staging `data/` directory, and write a manifest.
//!
//! Staging layout:
//! ```text
//! <staging>/
//! ├── manifest.json
//! └── data/                # mirrors the relevant subtree of ~/.claude
//!     ├── settings.json
//!     ├── CLAUDE.md
//!     └── projects/-home-user-x/session.jsonl
//! ```

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use sha2::{Digest, Sha256};
use walkdir::WalkDir;

use crate::config::Config;
use crate::error::CcError;
use crate::manifest::{FileEntry, Manifest, ProjectRoot};
use crate::mcp;
use crate::paths;
use crate::redact;

pub struct SnapshotOptions {
    pub dry_run: bool,
    pub allow_secrets: bool,
    /// `~/.claude.json` to harvest MCP server definitions from, when
    /// `config.include_mcp_servers` is set. `None` skips MCP bundling entirely.
    pub claude_json: Option<PathBuf>,
    /// Profile store to bundle under `ccsync-profiles/` in the snapshot, when
    /// `config.profiles.sync` is set. `None` skips profile bundling.
    pub profiles_root: Option<PathBuf>,
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
        let profiles_root = if config.profiles.sync {
            paths::profiles_dir().ok()
        } else {
            None
        };
        SnapshotOptions {
            dry_run,
            allow_secrets,
            claude_json,
            profiles_root,
        }
    }
}

/// File extensions we treat as text and therefore scan for secrets.
const SCANNED_EXTS: &[&str] = &["json", "toml", "md", "yaml", "yml", "env"];

/// Reports copy progress while a snapshot is built. Implemented by the CLI to
/// drive a progress bar; `snapshot::build` itself stays UI-agnostic.
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
    abs: PathBuf,
    rel: String,
    size: u64,
}

/// Build a snapshot of `claude_dir` into `staging`. Returns the manifest.
pub fn build(
    claude_dir: &Path,
    staging: &Path,
    config: &Config,
    opts: &SnapshotOptions,
) -> Result<Manifest> {
    build_inner(claude_dir, staging, config, opts, None)
}

/// Like [`build`], but reports per-file progress through `progress`.
pub fn build_with_progress(
    claude_dir: &Path,
    staging: &Path,
    config: &Config,
    opts: &SnapshotOptions,
    progress: &dyn ProgressSink,
) -> Result<Manifest> {
    build_inner(claude_dir, staging, config, opts, Some(progress))
}

fn build_inner(
    claude_dir: &Path,
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

    // Plan first: resolve the complete file list (applying include/exclude and
    // the credential hard-block) so progress has an accurate total before any
    // bytes are read or copied.
    let mut planned = Vec::new();
    for entry in &config.include {
        // `todos` is per-session state, gated together with the sessions.
        if (entry == "projects" || entry == "todos") && !config.include_sessions {
            continue;
        }
        let src = claude_dir.join(entry);
        if !src.exists() {
            continue;
        }
        plan_path(&src, claude_dir, "", config, &mut planned)?;
    }

    // Bundle the profile store under the reserved `ccsync-profiles/` name so
    // profiles ride along in snapshots; restore routes it back into the local
    // store. The machine-local `active.json` pointer never travels.
    if config.profiles.sync {
        if let Some(profiles_root) = opts.profiles_root.as_deref() {
            if profiles_root.is_dir() {
                plan_path(
                    profiles_root,
                    profiles_root,
                    crate::profile::PROFILES_COMPONENT,
                    config,
                    &mut planned,
                )?;
                let active = format!("{}/active.json", crate::profile::PROFILES_COMPONENT);
                planned.retain(|p| p.rel != active);
            }
        }
    }

    let total_bytes: u64 = planned.iter().map(|p| p.size).sum();
    if let Some(p) = progress {
        p.start(planned.len() as u64, total_bytes);
    }
    for pf in &planned {
        capture_file(pf, &data_root, config, opts, &mut manifest)?;
        if let Some(p) = progress {
            p.advance(pf.size);
        }
    }
    if let Some(p) = progress {
        p.finish();
    }

    // Record decoded project roots for remapping, even in dry-run. Dashes in
    // encoded names are ambiguous (separator vs literal), so resolve each name
    // against the real working directories this machine knows about: the
    // `projects` keys of `~/.claude.json` first, then the live filesystem,
    // with the naive decode as a last resort.
    if config.include_sessions {
        let projects = claude_dir.join("projects");
        if projects.is_dir() {
            let known = known_project_paths(opts.claude_json.as_deref());
            for child in fs::read_dir(&projects)? {
                let child = child?;
                if child.file_type()?.is_dir() {
                    let encoded = child.file_name().to_string_lossy().to_string();
                    let decoded_path = known
                        .get(&encoded)
                        .cloned()
                        .or_else(|| {
                            paths::resolve_encoded_on_disk(&encoded)
                                .map(|p| p.to_string_lossy().to_string())
                        })
                        .unwrap_or_else(|| {
                            paths::decode_path(&encoded).to_string_lossy().to_string()
                        });
                    manifest.project_roots.push(ProjectRoot {
                        decoded_path,
                        encoded,
                    });
                }
            }
        }
    }

    // Bundle locally-configured MCP servers from `~/.claude.json` (outside the
    // captured `~/.claude` tree) into a standalone file in the snapshot.
    if config.include_mcp_servers {
        if let Some(claude_json) = &opts.claude_json {
            capture_mcp_servers(claude_json, &data_root, opts, &mut manifest)?;
        }
    }

    if !opts.dry_run {
        manifest.write_to(staging)?;
    }
    Ok(manifest)
}

/// Map encoded project-directory names to the real working directories listed
/// in `~/.claude.json`'s `projects` object — the authoritative source, since
/// Claude Code derived the encoded names from exactly these paths.
fn known_project_paths(claude_json: Option<&Path>) -> std::collections::BTreeMap<String, String> {
    let mut known = std::collections::BTreeMap::new();
    let Some(cj) = claude_json else {
        return known;
    };
    let Ok(text) = fs::read_to_string(cj) else {
        return known;
    };
    let Ok(doc) = serde_json::from_str::<serde_json::Value>(&text) else {
        return known;
    };
    if let Some(projects) = doc.get("projects").and_then(|p| p.as_object()) {
        for key in projects.keys() {
            known.insert(paths::encode_path(Path::new(key)), key.clone());
        }
    }
    known
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
    });

    if !opts.dry_run {
        let dest = data_root.join(mcp::MCP_FILE);
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(&dest, &bytes).with_context(|| format!("writing {}", dest.display()))?;
    }
    Ok(())
}

/// Walk a single include entry (file or directory tree) and append the files
/// that survive include/exclude to `out`, with rel paths computed against
/// `base` and prefixed by `rel_prefix` (empty for the `~/.claude` walk;
/// `ccsync-profiles` for the bundled profile store). The credential
/// hard-block aborts the whole snapshot here, before any bytes are read.
fn plan_path(
    src: &Path,
    base: &Path,
    rel_prefix: &str,
    config: &Config,
    out: &mut Vec<PlannedFile>,
) -> Result<()> {
    for entry in WalkDir::new(src).follow_links(false) {
        let entry = entry?;
        if !entry.file_type().is_file() {
            continue;
        }
        let abs = entry.path();
        let mut rel = abs
            .strip_prefix(base)
            .expect("walked path is under its base")
            .to_string_lossy()
            .replace('\\', "/");
        if !rel_prefix.is_empty() {
            rel = format!("{rel_prefix}/{rel}");
        }

        let file_name = abs
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_default();

        // Hard block: credentials never leave the machine.
        if redact::is_credential_file(&file_name) {
            return Err(CcError::CredentialBlocked(rel).into());
        }

        if config.is_excluded(&rel) {
            continue;
        }

        let size = entry.metadata().map(|m| m.len()).unwrap_or(0);
        out.push(PlannedFile {
            abs: abs.to_path_buf(),
            rel,
            size,
        });
    }
    Ok(())
}

/// Scan, hash, and (unless dry-run) copy a single planned file into staging.
fn capture_file(
    pf: &PlannedFile,
    data_root: &Path,
    config: &Config,
    opts: &SnapshotOptions,
    manifest: &mut Manifest,
) -> Result<()> {
    use crate::config::TranscriptSecrets;

    let abs = pf.abs.as_path();
    let rel = &pf.rel;

    let mut bytes = fs::read(abs).with_context(|| format!("reading {}", abs.display()))?;

    // Secret handling unless explicitly allowed. Scanning goes through
    // `from_utf8_lossy` so a stray invalid byte cannot smuggle an otherwise
    // ASCII secret past the scan.
    if !opts.allow_secrets {
        if is_transcript(abs) {
            match config.transcript_secrets {
                TranscriptSecrets::Ignore => {}
                TranscriptSecrets::Redact => {
                    let text = String::from_utf8_lossy(&bytes);
                    if let Some((redacted, n)) = redact::redact_secrets(&text) {
                        manifest.redacted_spans += n as u64;
                        // Only the staged copy is rewritten; `abs` is untouched.
                        bytes = redacted.into_bytes();
                    }
                }
                TranscriptSecrets::Abort => {
                    if let Some(hint) = redact::scan_for_secrets(&String::from_utf8_lossy(&bytes)) {
                        return Err(CcError::SecretDetected {
                            file: rel.clone(),
                            hint,
                        }
                        .into());
                    }
                }
            }
        } else if is_scanned(abs) {
            if let Some(hint) = redact::scan_for_secrets(&String::from_utf8_lossy(&bytes)) {
                return Err(CcError::SecretDetected {
                    file: rel.clone(),
                    hint,
                }
                .into());
            }
        }
    }

    // Hash the (possibly redacted) bytes that actually land in staging, so
    // restore's integrity check matches.
    let sha256 = hex(&Sha256::digest(&bytes));
    manifest.files.push(FileEntry {
        rel_path: rel.clone(),
        sha256,
        size: bytes.len() as u64,
    });

    if !opts.dry_run {
        let dest = data_root.join(rel);
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

/// Session transcripts get the redact-don't-abort policy.
fn is_transcript(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| e.eq_ignore_ascii_case("jsonl"))
        .unwrap_or(false)
}

/// Top-level entries of `~/.claude` matched by neither `include` nor
/// `exclude`. These are silently dropped from snapshots — surfacing them lets
/// the user classify new Claude Code state instead of losing it unnoticed.
pub fn unclassified_top_level(claude_dir: &Path, config: &Config) -> Vec<String> {
    let Ok(entries) = fs::read_dir(claude_dir) else {
        return Vec::new();
    };
    let mut out: Vec<String> = entries
        .flatten()
        .filter_map(|e| e.file_name().to_str().map(str::to_string))
        .filter(|name| {
            !config
                .include
                .iter()
                .any(|i| i.trim_end_matches('/') == name)
                && !config.is_excluded(name)
        })
        .collect();
    out.sort();
    out
}

pub(crate) fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(s, "{b:02x}");
    }
    s
}

pub(crate) fn hostname() -> String {
    // $HOSTNAME is a non-exported shell variable on most Linux systems, so
    // env vars alone usually yield nothing; ask the OS directly first.
    #[cfg(unix)]
    {
        let mut buf = [0u8; 256];
        // SAFETY: buf is a valid, writable buffer of the stated length;
        // gethostname NUL-terminates on success.
        let rc = unsafe { libc::gethostname(buf.as_mut_ptr() as *mut libc::c_char, buf.len()) };
        if rc == 0 {
            let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
            if end > 0 {
                if let Ok(name) = std::str::from_utf8(&buf[..end]) {
                    return name.to_string();
                }
            }
        }
    }
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
            profiles_root: None,
        };
        let m = build(&claude, &staging, &cfg, &opts).unwrap();

        let rels: Vec<&str> = m.files.iter().map(|f| f.rel_path.as_str()).collect();
        assert!(rels.contains(&"settings.json"));
        assert!(rels.contains(&"CLAUDE.md"));
        assert!(rels.contains(&"projects/-home-alice-proj/sess.jsonl"));
        // Excluded machine-local state is not captured.
        assert!(!rels.iter().any(|r| r.starts_with("shell-snapshots")));
        // Project roots recorded for remapping.
        assert_eq!(m.project_roots.len(), 1);
        assert_eq!(m.project_roots[0].decoded_path, "/home/alice/proj");
        // Files actually landed in staging.
        assert!(staging.join("data/settings.json").exists());
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
            profiles_root: None,
        };
        let err = build(&claude, &staging, &cfg, &opts).unwrap_err();
        assert!(err.to_string().contains("credential"));
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
            profiles_root: None,
        };
        let err = build(&claude, &staging, &cfg, &opts).unwrap_err();
        assert!(err.to_string().contains("secret"));

        // With allow_secrets it succeeds.
        let opts = SnapshotOptions {
            dry_run: false,
            allow_secrets: true,
            claude_json: None,
            profiles_root: None,
        };
        assert!(build(&claude, &staging, &cfg, &opts).is_ok());
    }

    #[test]
    fn redacts_transcript_secrets_in_staged_copy_only() {
        let tmp = tempfile::tempdir().unwrap();
        let claude = tmp.path().join("claude");
        let staging = tmp.path().join("staging");
        let source = claude.join("projects/-home-a-p/sess.jsonl");
        let original = "{\"cwd\":\"/home/a/p\",\"paste\":\"sk-abcdefghijklmnopqrstuvwx\"}\n";
        write(&source, original);

        let cfg = Config::default();
        let opts = SnapshotOptions {
            dry_run: false,
            allow_secrets: false,
            claude_json: None,
            profiles_root: None,
        };
        let m = build(&claude, &staging, &cfg, &opts).unwrap();

        assert_eq!(m.redacted_spans, 1);
        let staged =
            fs::read_to_string(staging.join("data/projects/-home-a-p/sess.jsonl")).unwrap();
        assert!(!staged.contains("sk-abcdefghijklmnopqrstuvwx"));
        assert!(staged.contains(crate::redact::REDACTION_MARKER));
        // The source transcript is untouched.
        assert_eq!(fs::read_to_string(&source).unwrap(), original);
        // The manifest hash matches the redacted bytes that were staged.
        let entry = m
            .files
            .iter()
            .find(|f| f.rel_path.ends_with("sess.jsonl"))
            .unwrap();
        use sha2::Digest;
        assert_eq!(entry.sha256, hex(&Sha256::digest(staged.as_bytes())));

        // Abort policy behaves like config files do.
        let cfg = Config {
            transcript_secrets: crate::config::TranscriptSecrets::Abort,
            ..Config::default()
        };
        let err = build(&claude, &staging, &cfg, &opts).unwrap_err();
        assert!(err.to_string().contains("secret"));

        // Ignore policy captures verbatim.
        let cfg = Config {
            transcript_secrets: crate::config::TranscriptSecrets::Ignore,
            ..Config::default()
        };
        let m = build(&claude, &staging, &cfg, &opts).unwrap();
        assert_eq!(m.redacted_spans, 0);
        let staged =
            fs::read_to_string(staging.join("data/projects/-home-a-p/sess.jsonl")).unwrap();
        assert!(staged.contains("sk-abcdefghijklmnopqrstuvwx"));
    }

    #[test]
    fn scans_non_utf8_files_via_lossy_decode() {
        let tmp = tempfile::tempdir().unwrap();
        let claude = tmp.path().join("claude");
        let staging = tmp.path().join("staging");
        // One invalid byte used to skip the scan entirely; the embedded ASCII
        // key must still abort the snapshot.
        let mut bytes = b"{\"k\":\"sk-abcdefghijklmnopqrstuvwx\"}".to_vec();
        bytes.push(0xFF);
        fs::create_dir_all(&claude).unwrap();
        fs::write(claude.join("settings.json"), &bytes).unwrap();

        let cfg = Config::default();
        let opts = SnapshotOptions {
            dry_run: false,
            allow_secrets: false,
            claude_json: None,
            profiles_root: None,
        };
        let err = build(&claude, &staging, &cfg, &opts).unwrap_err();
        assert!(err.to_string().contains("secret"), "got: {err:#}");
    }

    #[test]
    fn reports_unclassified_top_level_entries() {
        let tmp = tempfile::tempdir().unwrap();
        let claude = tmp.path().join("claude");
        write(&claude.join("settings.json"), "{}");
        write(&claude.join("statsig/x"), "cache");
        write(&claude.join("some-new-state/data.json"), "{}");

        let cfg = Config::default();
        let unclassified = unclassified_top_level(&claude, &cfg);
        assert_eq!(unclassified, vec!["some-new-state".to_string()]);
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
            profiles_root: None,
        };
        let m = build(&claude, &staging, &cfg, &opts).unwrap();

        assert!(m.files.iter().any(|f| f.rel_path == crate::mcp::MCP_FILE));
        let staged = staging.join("data").join(crate::mcp::MCP_FILE);
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
        let staging = tmp.path().join("staging");
        write(&claude.join("settings.json"), r#"{"theme":"dark"}"#);
        write(&claude.join("CLAUDE.md"), "# memory");

        let cfg = Config::default();
        let opts = SnapshotOptions {
            dry_run: false,
            allow_secrets: false,
            claude_json: None,
            profiles_root: None,
        };
        let sink = CountingSink {
            files: Cell::new(0),
            bytes: Cell::new(0),
            advanced: Cell::new(0),
            finished: Cell::new(false),
        };
        let m = build_with_progress(&claude, &staging, &cfg, &opts, &sink).unwrap();

        let total: u64 = m.files.iter().map(|f| f.size).sum();
        assert_eq!(sink.files.get(), m.files.len() as u64);
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
            profiles_root: None,
        };
        let m = build(&claude, &staging, &cfg, &opts).unwrap();
        assert!(!m.files.iter().any(|f| f.rel_path == crate::mcp::MCP_FILE));
    }
}
