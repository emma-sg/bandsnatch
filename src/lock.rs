//! Cross-process mutual exclusion for runs.
//!
//! Two invocations writing the same library at once - a long scheduled run
//! overlapping a manual `docker exec bandsnatch release ...` - would race on
//! the per-release staging directory and the atomic swap into place. SQLite's
//! WAL and busy timeout protect the database; this protects the filesystem.

use std::error::Error;
use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};

/// Suffix appended to the state database path to get the lock path.
pub fn lock_path_for(state_path: &Path) -> PathBuf {
    let mut name = state_path.as_os_str().to_os_string();
    name.push(".lock");
    PathBuf::from(name)
}

/// An exclusive advisory lock held for as long as this value is alive.
///
/// The lock is released by the operating system when the file descriptor is
/// closed, so dropping this struct is sufficient and a crashed process cannot
/// leave a lock behind.
pub struct RunLock {
    // Held only to keep the descriptor open.
    _file: File,
}

impl RunLock {
    /// Take an exclusive lock, blocking until it is free unless `wait` is false.
    pub fn acquire(path: &Path, wait: bool) -> Result<Self, Box<dyn Error>> {
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)?;
            }
        }
        let file = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(false)
            .open(path)?;
        lock_file(&file, wait, path)?;
        Ok(Self { _file: file })
    }
}

#[cfg(unix)]
fn lock_file(file: &File, wait: bool, path: &Path) -> Result<(), Box<dyn Error>> {
    use std::os::unix::io::AsRawFd;

    let mut flags = libc::LOCK_EX;
    if !wait {
        flags |= libc::LOCK_NB;
    }

    // SAFETY: `file` owns an open descriptor that outlives this call, and flock
    // only inspects that descriptor.
    let rc = unsafe { libc::flock(file.as_raw_fd(), flags) };
    if rc == 0 {
        return Ok(());
    }

    let err = std::io::Error::last_os_error();
    if !wait && err.raw_os_error() == Some(libc::EWOULDBLOCK) {
        return Err(format!(
            "another bandsnatch run already holds {}; wait for it to finish or drop --no-wait",
            path.display()
        )
        .into());
    }
    Err(err.into())
}

/// Advisory locking is implemented for unix only. Scheduled runs happen in
/// containers or from cron; Windows usage of this tool is interactive, so the
/// missing lock is a documented limitation rather than a silent one.
#[cfg(not(unix))]
fn lock_file(_file: &File, _wait: bool, _path: &Path) -> Result<(), Box<dyn Error>> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lock_path_is_derived_from_the_state_path() {
        assert_eq!(
            lock_path_for(Path::new("/music/.bandsnatch-state.db")),
            PathBuf::from("/music/.bandsnatch-state.db.lock")
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_second_non_blocking_acquire_reports_contention() {
        let dir = std::env::temp_dir().join(format!(
            "bandsnatch-lock-{}-{}",
            std::process::id(),
            rand::random::<u64>()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("state.db.lock");

        let first = RunLock::acquire(&path, false).unwrap();
        // A second lock on the same file from the same process still contends:
        // flock is per open file description, not per process or per thread.
        let second = RunLock::acquire(&path, false);
        assert!(second.is_err(), "second acquire should have failed");

        drop(first);
        // Once released, the lock is available again.
        let third = RunLock::acquire(&path, false);
        assert!(third.is_ok());

        std::fs::remove_dir_all(&dir).unwrap();
    }
}
