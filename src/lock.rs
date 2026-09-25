//! Advisory file locks serializing ccsync processes (CLI, TUI, daemon) that
//! share on-disk state such as the git repo cache or the daemon pidfile.

use std::path::Path;

use anyhow::{Context, Result};

/// Block until an exclusive advisory lock on `path` (created if absent) is
/// held; it is released when the returned file is dropped. `flock` locks
/// belong to the open file, so never take the same lock twice in one call
/// chain — the second attempt would wait on the first forever.
#[cfg(unix)]
pub fn exclusive(path: &Path) -> Result<std::fs::File> {
    use std::os::unix::io::AsRawFd;

    let file = open(path)?;
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } != 0 {
        return Err(std::io::Error::last_os_error())
            .with_context(|| format!("locking {}", path.display()));
    }
    Ok(file)
}

/// No advisory locking off Unix; the file is still created so callers behave
/// the same.
#[cfg(not(unix))]
pub fn exclusive(path: &Path) -> Result<std::fs::File> {
    open(path)
}

fn open(path: &Path) -> Result<std::fs::File> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(path)
        .with_context(|| format!("opening {}", path.display()))
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::time::Duration;

    #[test]
    fn exclusive_lock_blocks_until_released() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("state.lock");
        let held = exclusive(&path).unwrap();

        let (tx, rx) = mpsc::channel();
        let p = path.clone();
        let waiter = std::thread::spawn(move || {
            let _second = exclusive(&p).unwrap();
            tx.send(()).unwrap();
        });
        assert!(
            rx.recv_timeout(Duration::from_millis(200)).is_err(),
            "second lock acquired while the first was held"
        );
        drop(held);
        rx.recv_timeout(Duration::from_secs(5))
            .expect("second lock acquired after release");
        waiter.join().unwrap();
    }
}
