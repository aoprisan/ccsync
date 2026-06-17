//! Crate-wide error type. We lean on `anyhow` for application-level error
//! propagation and reserve this enum for the few conditions callers may want
//! to match on (e.g. aborting because secrets were detected).

use thiserror::Error;

#[derive(Debug, Error)]
pub enum CcError {
    #[error(
        "could not locate the Claude Code directory (~/.claude); set CLAUDE_CONFIG_DIR or HOME"
    )]
    ClaudeDirNotFound,

    #[error("no snapshot found in the staging directory ({0}); run `ccsync snapshot` or `ccsync pull` first")]
    NoStagedSnapshot(String),

    #[error("refusing to include credential file: {0}")]
    CredentialBlocked(String),

    #[error("potential secret detected in {file}: {hint}\n  re-run with --allow-secrets to include it anyway")]
    SecretDetected { file: String, hint: String },

    #[error("git command failed: {0}")]
    Git(String),

    #[error("no remote configured; set `remote` in the config or pass --archive")]
    NoRemote,

    // Only constructed on platforms without a supported service manager
    // (i.e. not Linux/macOS), but compiled on all targets.
    #[allow(dead_code)]
    #[error("the background service is not supported on this platform; `ccsync daemon` still runs in the foreground")]
    ServiceUnsupportedPlatform,
}
