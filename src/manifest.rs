//! The snapshot manifest. Every snapshot carries a `manifest.json` at its root
//! describing what it contains and where it came from. The manifest is what
//! makes path remapping possible on restore: it records the source machine's
//! home directory and the decoded original working directory for each captured
//! project session directory.
//!
//! Version history:
//! - **1**: single-tool (Claude only), files staged flat under `data/`.
//! - **2**: multi-tool; files staged under `data/<tool>/`, each `FileEntry`
//!   tagged with its tool, and a `tools` list recording each captured source
//!   root. v1 manifests still load: untagged entries default to Claude and
//!   [`Manifest::tool_data_root`] resolves to the flat layout.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::CcError;
use crate::tools::ToolId;

/// File name of the manifest stored at the root of a snapshot.
pub const MANIFEST_NAME: &str = "manifest.json";

/// Newest manifest version this binary can restore. Reading a newer snapshot
/// fails loudly instead of mis-routing files written by a future layout.
pub const MAX_SUPPORTED_VERSION: u32 = 2;

/// Top-level manifest written into every snapshot.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Manifest {
    /// Schema version so future ccsync versions can migrate older snapshots.
    pub manifest_version: u32,
    /// ccsync version that produced the snapshot.
    pub ccsync_version: String,
    /// Hostname of the source machine (informational).
    pub source_host: String,
    /// Absolute home directory on the source machine. Used as the default
    /// remap source prefix.
    pub source_home: String,
    /// ISO-8601 creation timestamp.
    pub created_at: String,
    /// Every regular file captured. Paths are relative to the tool's data
    /// root (see [`Manifest::tool_data_root`]), with a SHA-256 for integrity
    /// verification.
    pub files: Vec<FileEntry>,
    /// One entry per captured Claude session project directory, recording the
    /// encoded directory name and the decoded absolute working directory it
    /// represents. Claude-specific: Copilot has no cwd-encoded layout.
    #[serde(default)]
    pub project_roots: Vec<ProjectRoot>,
    /// The tools captured in this snapshot and their source roots. Empty in
    /// v1 manifests, which are Claude-only by definition.
    #[serde(default)]
    pub tools: Vec<ToolEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileEntry {
    /// Path relative to the tool's data root (forward slashes).
    pub rel_path: String,
    pub sha256: String,
    pub size: u64,
    /// Which tool this file belongs to. v1 entries lack the field and default
    /// to Claude.
    #[serde(default)]
    pub tool: ToolId,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectRoot {
    /// The encoded directory name as found under `projects/`.
    pub encoded: String,
    /// The decoded absolute working directory it corresponds to.
    pub decoded_path: String,
}

/// A tool captured in the snapshot.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolEntry {
    pub tool: ToolId,
    /// Absolute root the tool was captured from on the source machine
    /// (e.g. `/Users/alice/.copilot`). Informational.
    pub source_root: String,
}

impl Manifest {
    pub fn current_version() -> u32 {
        2
    }

    pub fn new(source_host: String, source_home: String) -> Self {
        Manifest {
            manifest_version: Self::current_version(),
            ccsync_version: env!("CARGO_PKG_VERSION").to_string(),
            source_host,
            source_home,
            created_at: chrono::Utc::now().to_rfc3339(),
            files: Vec::new(),
            project_roots: Vec::new(),
            tools: Vec::new(),
        }
    }

    /// The distinct tools this snapshot carries, in `ToolId::all()` order.
    /// A v1 manifest is Claude-only by definition; a v2 manifest reports its
    /// `tools` list, falling back to the per-file tags for robustness.
    pub fn tools_present(&self) -> Vec<ToolId> {
        if self.manifest_version < 2 {
            return vec![ToolId::Claude];
        }
        let mut present: Vec<ToolId> = Vec::new();
        for &id in ToolId::all() {
            let listed =
                self.tools.iter().any(|t| t.tool == id) || self.files.iter().any(|f| f.tool == id);
            if listed {
                present.push(id);
            }
        }
        present
    }

    /// Where `tool`'s files live inside the staged snapshot: the flat `data/`
    /// root for v1 (Claude-only) snapshots, `data/<tool>/` from v2 on.
    pub fn tool_data_root(&self, staging: &Path, tool: ToolId) -> PathBuf {
        let data = staging.join("data");
        if self.manifest_version < 2 {
            data
        } else {
            data.join(tool.as_str())
        }
    }

    pub fn write_to(&self, dir: &Path) -> anyhow::Result<()> {
        let path = dir.join(MANIFEST_NAME);
        let json = serde_json::to_string_pretty(self)?;
        std::fs::write(path, json)?;
        Ok(())
    }

    pub fn read_from(dir: &Path) -> anyhow::Result<Self> {
        let path = dir.join(MANIFEST_NAME);
        let json = std::fs::read_to_string(path)?;
        let manifest: Manifest = serde_json::from_str(&json)?;
        if manifest.manifest_version > MAX_SUPPORTED_VERSION {
            return Err(CcError::ManifestTooNew {
                found: manifest.manifest_version,
                max: MAX_SUPPORTED_VERSION,
            }
            .into());
        }
        Ok(manifest)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifest_roundtrips_through_json() {
        let tmp = tempfile::tempdir().unwrap();
        let mut m = Manifest::new("host1".into(), "/home/alice".into());
        m.files.push(FileEntry {
            rel_path: "settings.json".into(),
            sha256: "abc".into(),
            size: 12,
            tool: ToolId::Claude,
        });
        m.files.push(FileEntry {
            rel_path: "mcp-config.json".into(),
            sha256: "def".into(),
            size: 8,
            tool: ToolId::Copilot,
        });
        m.project_roots.push(ProjectRoot {
            encoded: "-home-alice-proj".into(),
            decoded_path: "/home/alice/proj".into(),
        });
        m.tools.push(ToolEntry {
            tool: ToolId::Claude,
            source_root: "/home/alice/.claude".into(),
        });
        m.tools.push(ToolEntry {
            tool: ToolId::Copilot,
            source_root: "/home/alice/.copilot".into(),
        });
        m.write_to(tmp.path()).unwrap();
        let read = Manifest::read_from(tmp.path()).unwrap();
        assert_eq!(read.source_home, "/home/alice");
        assert_eq!(read.files.len(), 2);
        assert_eq!(read.files[1].tool, ToolId::Copilot);
        assert_eq!(read.project_roots[0].encoded, "-home-alice-proj");
        assert_eq!(read.tools_present(), vec![ToolId::Claude, ToolId::Copilot]);
    }

    #[test]
    fn v1_manifest_loads_as_claude_only() {
        // A literal manifest as written by ccsync before multi-tool support:
        // no per-file `tool` tags, no `tools` list, flat data/ layout.
        let v1 = r#"{
            "manifest_version": 1,
            "ccsync_version": "0.1.0",
            "source_host": "host1",
            "source_home": "/home/alice",
            "created_at": "2026-01-01T00:00:00Z",
            "files": [
                {"rel_path": "settings.json", "sha256": "abc", "size": 12}
            ],
            "project_roots": [
                {"encoded": "-home-alice-proj", "decoded_path": "/home/alice/proj"}
            ]
        }"#;
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join(MANIFEST_NAME), v1).unwrap();
        let m = Manifest::read_from(tmp.path()).unwrap();

        assert_eq!(m.files[0].tool, ToolId::Claude);
        assert_eq!(m.tools_present(), vec![ToolId::Claude]);
        // v1 snapshots stage files flat under data/, for every tool query.
        let staging = Path::new("/staging");
        assert_eq!(
            m.tool_data_root(staging, ToolId::Claude),
            staging.join("data")
        );
    }

    #[test]
    fn v2_layout_is_per_tool() {
        let m = Manifest::new("h".into(), "/home/a".into());
        let staging = Path::new("/staging");
        assert_eq!(
            m.tool_data_root(staging, ToolId::Claude),
            staging.join("data").join("claude")
        );
        assert_eq!(
            m.tool_data_root(staging, ToolId::Copilot),
            staging.join("data").join("copilot")
        );
    }

    #[test]
    fn rejects_manifests_from_the_future() {
        let v3 = r#"{
            "manifest_version": 3,
            "ccsync_version": "9.9.9",
            "source_host": "h",
            "source_home": "/home/x",
            "created_at": "2030-01-01T00:00:00Z",
            "files": []
        }"#;
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join(MANIFEST_NAME), v3).unwrap();
        let err = Manifest::read_from(tmp.path()).unwrap_err();
        assert!(err.to_string().contains("newer than this ccsync supports"));
    }
}
