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

    // Encrypt the tarball with age into a temp file beside `out`, and only
    // rename it into place once fully written: a failure midway must never
    // truncate or half-overwrite an existing archive (possibly the only
    // backup).
    let encryptor = age::Encryptor::with_user_passphrase(Secret::new(passphrase.to_owned()));
    let parent = match out.parent() {
        Some(p) if !p.as_os_str().is_empty() => p.to_path_buf(),
        _ => std::path::PathBuf::from("."),
    };
    std::fs::create_dir_all(&parent)?;
    let mut tmp = tempfile::Builder::new()
        .prefix(".ccsync-archive-")
        .tempfile_in(&parent)
        .with_context(|| format!("creating temp archive in {}", parent.display()))?;
    {
        let mut writer = encryptor.wrap_output(tmp.as_file_mut())?;
        writer.write_all(&tar_gz)?;
        writer.finish()?;
    }
    tmp.as_file().sync_all()?;
    tmp.persist(out)
        .map_err(|e| anyhow!("writing archive {}: {}", out.display(), e.error))?;
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

    // Unpack beside `staging` and swap it in only once the whole archive has
    // validated: a corrupt or hostile archive must leave the existing staged
    // snapshot untouched.
    crate::snapshot::replace_staging(staging, |fresh| unpack_checked(&tar_gz, fresh))
}

/// Unpack a gzip tarball into `dest` entry by entry so a hostile archive
/// cannot escape it (absolute paths, `..`, or link entries pointing
/// elsewhere).
fn unpack_checked(tar_gz: &[u8], dest: &Path) -> Result<()> {
    let staging = dest;
    let gz = GzDecoder::new(tar_gz);
    let mut archive = tar::Archive::new(gz);
    for entry in archive.entries()? {
        let mut entry = entry?;
        let path = entry.path()?.into_owned();
        let safe_path = path
            .components()
            .all(|c| matches!(c, std::path::Component::Normal(_)));
        let kind = entry.header().entry_type();
        let safe_kind = matches!(kind, tar::EntryType::Regular | tar::EntryType::Directory);
        if !safe_path || !safe_kind {
            return Err(anyhow!(
                "refusing unsafe archive entry {} ({kind:?})",
                path.display()
            ));
        }
        // unpack_in re-checks that the destination stays under `staging`.
        entry.unpack_in(staging)?;
    }
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
    fn rejects_link_entries() {
        // Hand-build an encrypted archive holding a symlink entry; extract
        // must refuse it rather than materialize a link out of staging.
        let mut tar_gz: Vec<u8> = Vec::new();
        {
            let enc = GzEncoder::new(&mut tar_gz, Compression::default());
            let mut builder = tar::Builder::new(enc);
            let mut header = tar::Header::new_gnu();
            header.set_entry_type(tar::EntryType::Symlink);
            header.set_size(0);
            builder
                .append_link(&mut header, "data/evil", "/etc/passwd")
                .unwrap();
            builder.into_inner().unwrap().finish().unwrap();
        }
        let tmp = tempfile::tempdir().unwrap();
        let out = tmp.path().join("evil.tar.gz.age");
        let encryptor = age::Encryptor::with_user_passphrase(Secret::new("hunter2".to_owned()));
        let mut writer = encryptor
            .wrap_output(std::fs::File::create(&out).unwrap())
            .unwrap();
        writer.write_all(&tar_gz).unwrap();
        writer.finish().unwrap();

        let staging = tmp.path().join("staging");
        std::fs::create_dir_all(&staging).unwrap();
        std::fs::write(staging.join(MANIFEST_NAME), "existing").unwrap();
        let err = extract(&out, &staging, "hunter2").unwrap_err();
        assert!(err.to_string().contains("unsafe"), "got: {err:#}");
        assert!(!staging.join("data/evil").exists());
        // The rejected archive left the previous staging in place.
        assert_eq!(
            std::fs::read_to_string(staging.join(MANIFEST_NAME)).unwrap(),
            "existing"
        );
    }

    /// Every entry in `dir` other than `keep`, to catch leaked temp files.
    fn stray_entries(dir: &Path, keep: &[&str]) -> Vec<String> {
        std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().to_string())
            .filter(|n| !keep.contains(&n.as_str()))
            .collect()
    }

    #[test]
    fn corrupt_archive_leaves_existing_staging_intact() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src");
        std::fs::create_dir_all(src.join("data")).unwrap();
        std::fs::write(src.join(MANIFEST_NAME), r#"{"k":1}"#).unwrap();
        std::fs::write(src.join("data/settings.json"), "good").unwrap();
        let good = tmp.path().join("good.tar.gz.age");
        create(&src, &good, "pw").unwrap();

        let staging = tmp.path().join("staging");
        extract(&good, &staging, "pw").unwrap();

        // Truncated ciphertext: decryption/unpack fails partway.
        let bytes = std::fs::read(&good).unwrap();
        let bad = tmp.path().join("bad.tar.gz.age");
        std::fs::write(&bad, &bytes[..bytes.len() / 2]).unwrap();
        assert!(extract(&bad, &staging, "pw").is_err());
        // Wrong passphrase also fails without touching staging.
        assert!(extract(&good, &staging, "nope").is_err());

        assert_eq!(
            std::fs::read_to_string(staging.join("data/settings.json")).unwrap(),
            "good"
        );
        assert!(stray_entries(
            tmp.path(),
            &["src", "staging", "good.tar.gz.age", "bad.tar.gz.age"]
        )
        .is_empty());
    }

    #[test]
    fn create_replaces_output_atomically() {
        let tmp = tempfile::tempdir().unwrap();
        let out_dir = tmp.path().join("out");
        std::fs::create_dir_all(&out_dir).unwrap();
        let out = out_dir.join("snap.tar.gz.age");
        std::fs::write(&out, "previous backup").unwrap();

        // A failing create (no manifest to pack) leaves the old archive.
        let empty = tmp.path().join("empty");
        std::fs::create_dir_all(&empty).unwrap();
        assert!(create(&empty, &out, "pw").is_err());
        assert_eq!(std::fs::read_to_string(&out).unwrap(), "previous backup");

        // A successful create replaces it and leaves no temp files behind.
        let staging = tmp.path().join("staging");
        std::fs::create_dir_all(&staging).unwrap();
        std::fs::write(staging.join(MANIFEST_NAME), "{}").unwrap();
        create(&staging, &out, "pw").unwrap();
        let restored = tmp.path().join("restored");
        extract(&out, &restored, "pw").unwrap();
        assert_eq!(
            std::fs::read_to_string(restored.join(MANIFEST_NAME)).unwrap(),
            "{}"
        );
        assert!(stray_entries(&out_dir, &["snap.tar.gz.age"]).is_empty());
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
