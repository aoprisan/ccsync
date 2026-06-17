//! `ccsync install`: copy the running binary into a `bin` directory on the
//! user's PATH. We copy rather than symlink so the installed tool keeps working
//! after a `cargo clean` or once the source tree moves — the install is a
//! snapshot.

use std::env;
use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};

/// Install the running executable into a user bin directory on `$PATH`.
pub fn install() -> Result<()> {
    let source = env::current_exe().context("locating the running executable")?;
    // Resolve symlinks so we copy the real binary, not a link back into target/.
    let source = fs::canonicalize(&source).unwrap_or(source);

    let file_name = source
        .file_name()
        .ok_or_else(|| anyhow!("executable path has no file name: {}", source.display()))?
        .to_os_string();

    let home = crate::paths::home_dir().context("locating your home directory")?;
    let path_var = env::var_os("PATH").unwrap_or_default();

    let target_dir = choose_install_dir(&home, &path_var);
    fs::create_dir_all(&target_dir)
        .with_context(|| format!("creating install directory {}", target_dir.display()))?;
    let dest = target_dir.join(&file_name);

    if dest == source {
        bail!(
            "{} is already the installed binary — build a fresh copy and run `ccsync install` from there",
            dest.display()
        );
    }

    copy_binary(&source, &dest)?;
    println!("Installed {} -> {}", source.display(), dest.display());

    if !path_contains(&path_var, &target_dir) {
        eprintln!();
        eprintln!("warning: {} is not on your PATH.", target_dir.display());
        eprintln!("Add it to your shell profile, e.g.:");
        eprintln!("    export PATH=\"{}:$PATH\"", target_dir.display());
    }
    Ok(())
}

/// Atomically place `source` at `dest`: copy to a sibling temp file, then rename
/// over the destination. The rename is atomic within the directory and can
/// replace a binary that is currently running. `fs::copy` carries the source's
/// mode bits across, so the installed file stays executable.
fn copy_binary(source: &Path, dest: &Path) -> Result<()> {
    let stem = dest.file_name().and_then(OsStr::to_str).unwrap_or("ccsync");
    let tmp = dest.with_file_name(format!("{stem}.install-tmp"));
    fs::copy(source, &tmp)
        .with_context(|| format!("copying {} to {}", source.display(), tmp.display()))?;
    fs::rename(&tmp, dest).with_context(|| format!("installing into {}", dest.display()))?;
    Ok(())
}

/// Pick a destination bin directory: the first conventional user bin dir that is
/// already on `$PATH` (so the install works immediately), else `~/.local/bin`.
fn choose_install_dir(home: &Path, path_var: &OsStr) -> PathBuf {
    let preferred = [home.join(".local/bin"), home.join("bin"), home.join(".cargo/bin")];
    for dir in &preferred {
        if path_contains(path_var, dir) {
            return dir.clone();
        }
    }
    home.join(".local/bin")
}

/// Does `$PATH` contain `dir`? Compares canonicalized paths where possible so a
/// symlinked equivalent still matches; falls back to a literal compare for paths
/// that do not exist yet.
fn path_contains(path_var: &OsStr, dir: &Path) -> bool {
    let target = fs::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf());
    env::split_paths(path_var).any(|p| fs::canonicalize(&p).unwrap_or(p) == target)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn copy_binary_overwrites_and_preserves_executable_bit() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("ccsync-build");
        fs::write(&source, b"#!/bin/sh\necho hi\n").unwrap();
        fs::set_permissions(&source, fs::Permissions::from_mode(0o755)).unwrap();

        let bin = dir.path().join("bin");
        fs::create_dir_all(&bin).unwrap();
        let dest = bin.join("ccsync");
        fs::write(&dest, b"stale").unwrap(); // a previous install to replace

        copy_binary(&source, &dest).unwrap();

        assert_eq!(fs::read(&dest).unwrap(), b"#!/bin/sh\necho hi\n");
        let mode = fs::metadata(&dest).unwrap().permissions().mode();
        assert_eq!(mode & 0o111, 0o111, "executable bits should be preserved");

        let leftover_tmp = fs::read_dir(&bin)
            .unwrap()
            .filter_map(Result::ok)
            .any(|e| e.file_name().to_string_lossy().contains("install-tmp"));
        assert!(!leftover_tmp, "temp file should be renamed away, not left behind");
    }

    #[test]
    fn prefers_a_dir_already_on_path() {
        let home = PathBuf::from("/home/dev");
        let path_var =
            env::join_paths([PathBuf::from("/usr/bin"), home.join(".cargo/bin")]).unwrap();
        // ~/.local/bin and ~/bin are absent from PATH, so ~/.cargo/bin wins.
        assert_eq!(choose_install_dir(&home, &path_var), home.join(".cargo/bin"));
    }

    #[test]
    fn falls_back_to_local_bin_when_none_on_path() {
        let home = PathBuf::from("/home/dev");
        let path_var = env::join_paths([PathBuf::from("/usr/bin")]).unwrap();
        assert_eq!(choose_install_dir(&home, &path_var), home.join(".local/bin"));
    }
}
