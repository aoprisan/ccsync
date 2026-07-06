//! The snapshot manifest. Every snapshot carries a `manifest.json` at its root
//! describing what it contains and where it came from. The manifest is what
//! makes path remapping possible on restore: it records the source machine's
//! home directory and the decoded original working directory for each captured
//! project session directory.

use std::path::Path;

use serde::{Deserialize, Serialize};

/// File name of the manifest stored at the root of a snapshot.
pub const MANIFEST_NAME: &str = "manifest.json";

/// Newest manifest version this binary can restore. Reading a newer snapshot
/// fails loudly instead of silently mis-restoring a layout this binary does
/// not understand.
///
/// Version history: **1** — Claude-only `data/`; **2** — identical schema, but
/// `data/` may carry the reserved `ccsync-copilot/` component (routed into
/// `~/.copilot` on restore).
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
    /// Every regular file captured, relative to the snapshot's `data/` root,
    /// with its SHA-256 for integrity verification.
    pub files: Vec<FileEntry>,
    /// One entry per captured session project directory, recording the encoded
    /// directory name and the decoded absolute working directory it represents.
    pub project_roots: Vec<ProjectRoot>,
    /// Number of secret-shaped spans redacted from transcripts in the staged
    /// copy (informational; absent in pre-redaction manifests).
    #[serde(default)]
    pub redacted_spans: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileEntry {
    /// Path relative to the snapshot `data/` directory (forward slashes).
    pub rel_path: String,
    pub sha256: String,
    pub size: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectRoot {
    /// The encoded directory name as found under `projects/`.
    pub encoded: String,
    /// The decoded absolute working directory it corresponds to.
    pub decoded_path: String,
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
            redacted_spans: 0,
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
            return Err(crate::error::CcError::ManifestTooNew {
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
        });
        m.project_roots.push(ProjectRoot {
            encoded: "-home-alice-proj".into(),
            decoded_path: "/home/alice/proj".into(),
        });
        m.write_to(tmp.path()).unwrap();
        let read = Manifest::read_from(tmp.path()).unwrap();
        assert_eq!(read.source_home, "/home/alice");
        assert_eq!(read.files.len(), 1);
        assert_eq!(read.project_roots[0].encoded, "-home-alice-proj");
    }

    #[test]
    fn v1_manifest_still_loads() {
        // A literal manifest as written before the version bump.
        let v1 = r#"{
            "manifest_version": 1,
            "ccsync_version": "0.1.0",
            "source_host": "host1",
            "source_home": "/home/alice",
            "created_at": "2026-01-01T00:00:00Z",
            "files": [{"rel_path": "settings.json", "sha256": "abc", "size": 12}],
            "project_roots": []
        }"#;
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join(MANIFEST_NAME), v1).unwrap();
        let m = Manifest::read_from(tmp.path()).unwrap();
        assert_eq!(m.manifest_version, 1);
        assert_eq!(m.files.len(), 1);
    }

    #[test]
    fn rejects_manifests_from_the_future() {
        let v99 = r#"{
            "manifest_version": 99,
            "ccsync_version": "9.9.9",
            "source_host": "h",
            "source_home": "/home/x",
            "created_at": "2030-01-01T00:00:00Z",
            "files": [],
            "project_roots": []
        }"#;
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join(MANIFEST_NAME), v99).unwrap();
        let err = Manifest::read_from(tmp.path()).unwrap_err();
        assert!(err.to_string().contains("newer than this ccsync supports"));
    }
}
