//! Encrypted archive transport. A snapshot's staging directory (manifest +
//! `data/`) is packed into a gzip-compressed tarball and then encrypted with
//! `age` using a passphrase. Transcripts can contain sensitive content, so the
//! archive is always encrypted — there is no plaintext mode.
//!
//! The passphrase is read from the `CCSYNC_PASSPHRASE` environment variable to
//! keep the tool free of interactive/tty dependencies (suitable for CI and
//! scripted backups).

use std::io::{Read, Write};
use std::path::Path;

use age::secrecy::Secret;
use anyhow::{anyhow, Context, Result};
use flate2::read::GzDecoder;
use flate2::write::GzEncoder;
use flate2::Compression;

use crate::manifest::MANIFEST_NAME;

const PASSPHRASE_ENV: &str = "CCSYNC_PASSPHRASE";

/// Read the archive passphrase from the environment.
pub fn passphrase_from_env() -> Result<String> {
    match std::env::var(PASSPHRASE_ENV) {
        Ok(p) if !p.is_empty() => Ok(p),
        _ => Err(anyhow!(
            "set {PASSPHRASE_ENV} to a passphrase to encrypt/decrypt the archive"
        )),
    }
}

/// Pack `staging` (its `manifest.json` and `data/` subtree) into an encrypted
/// `.tar.gz.age` archive at `out`.
pub fn create(staging: &Path, out: &Path, passphrase: &str) -> Result<()> {
    // Build the gzip tarball in memory.
    let mut tar_gz: Vec<u8> = Vec::new();
    {
        let enc = GzEncoder::new(&mut tar_gz, Compression::default());
        let mut builder = tar::Builder::new(enc);

        let manifest_path = staging.join(MANIFEST_NAME);
        builder
            .append_path_with_name(&manifest_path, MANIFEST_NAME)
            .with_context(|| format!("adding {} to archive", manifest_path.display()))?;

        let data = staging.join("data");
        if data.exists() {
            builder
                .append_dir_all("data", &data)
                .with_context(|| format!("adding {} to archive", data.display()))?;
        }
        let enc = builder.into_inner()?;
        enc.finish()?;
    }

    // Encrypt the tarball with age.
    let encryptor = age::Encryptor::with_user_passphrase(Secret::new(passphrase.to_owned()));
    if let Some(parent) = out.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let out_file = std::fs::File::create(out)
        .with_context(|| format!("creating archive {}", out.display()))?;
    let mut writer = encryptor.wrap_output(out_file)?;
    writer.write_all(&tar_gz)?;
    writer.finish()?;
    Ok(())
}

/// Decrypt and unpack an archive at `archive` into the `staging` directory,
/// replacing any existing staged snapshot.
pub fn extract(archive: &Path, staging: &Path, passphrase: &str) -> Result<()> {
    let file = std::fs::File::open(archive)
        .with_context(|| format!("opening archive {}", archive.display()))?;

    let decryptor = match age::Decryptor::new(file)? {
        age::Decryptor::Passphrase(d) => d,
        _ => return Err(anyhow!("archive is not passphrase-encrypted")),
    };
    let mut reader = decryptor
        .decrypt(&Secret::new(passphrase.to_owned()), None)
        .map_err(|e| anyhow!("decryption failed (wrong passphrase?): {e}"))?;

    let mut tar_gz = Vec::new();
    reader.read_to_end(&mut tar_gz)?;

    // Clean and recreate staging, then unpack.
    if staging.exists() {
        std::fs::remove_dir_all(staging).ok();
    }
    std::fs::create_dir_all(staging)?;
    let gz = GzDecoder::new(&tar_gz[..]);
    let mut archive = tar::Archive::new(gz);
    archive.unpack(staging)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn archive_roundtrips() {
        let tmp = tempfile::tempdir().unwrap();
        let staging = tmp.path().join("staging");
        std::fs::create_dir_all(staging.join("data")).unwrap();
        std::fs::write(staging.join(MANIFEST_NAME), r#"{"k":1}"#).unwrap();
        std::fs::write(staging.join("data/settings.json"), r#"{"theme":"dark"}"#).unwrap();

        let out = tmp.path().join("snap.tar.gz.age");
        create(&staging, &out, "hunter2").unwrap();
        assert!(out.exists());

        let restored = tmp.path().join("restored");
        extract(&out, &restored, "hunter2").unwrap();
        assert!(restored.join(MANIFEST_NAME).exists());
        let s = std::fs::read_to_string(restored.join("data/settings.json")).unwrap();
        assert!(s.contains("dark"));
    }

    #[test]
    fn wrong_passphrase_fails() {
        let tmp = tempfile::tempdir().unwrap();
        let staging = tmp.path().join("staging");
        std::fs::create_dir_all(&staging).unwrap();
        std::fs::write(staging.join(MANIFEST_NAME), "{}").unwrap();
        let out = tmp.path().join("snap.tar.gz.age");
        create(&staging, &out, "right").unwrap();
        let restored = tmp.path().join("restored");
        assert!(extract(&out, &restored, "wrong").is_err());
    }
}
