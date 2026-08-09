//! The exclusive lock that keeps an installation from replacing files
//! underneath a running launcher (spec 23.3).
//!
//! §23.3 requires that a plugin's processes stop before its files are
//! replaced. CriKey cannot do that across a process boundary: `crikey plugin
//! install` runs in its own process, the plugin's children belong to a
//! launcher started separately, and this workspace has no inter-process
//! channel to ask that launcher for anything. What it *can* do is guarantee
//! that no launcher is running while an installation swaps directories, which
//! is what §23.3 is protecting against.
//!
//! The mechanism is an operating-system exclusive lock, not a pid file. A pid
//! written to a file is racy in both directions: a launcher may start between
//! the check and the swap, and a recycled pid makes a dead launcher read as
//! live. An OS lock has neither problem — the kernel releases it when the
//! holding process dies, however it dies, so there is no stale state to
//! reclaim and no liveness heuristic to get wrong, and a launcher that tries to
//! start mid-installation blocks on the same lock.
//!
//! The lock is held for the *whole* replacement, not consulted as a
//! pre-flight check, and it fails closed: a state directory that cannot be
//! created or a lock file that cannot be opened refuses the installation. The
//! one outcome worse than refusing is proceeding while detection is broken,
//! because that looks like success.
//!
//! A pid is recorded beside the lock purely so the refusal can name the
//! process the user has to quit. It is diagnostic text and never the safety
//! mechanism.

use std::fs::{self, File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};

use crikey_platform::StandardDirectories;

use crate::PackageError;

/// File whose lock means "a launcher owns the installed plugins".
const LOCK_FILE: &str = "launcher.lock";

/// A pid is optional diagnostics only. It is created with `create_new`, so an
/// existing symlink is never followed or truncated.
const PID_FILE: &str = "launcher.pid";

/// An acquired exclusive launcher lock.
///
/// Held for as long as the value lives, and released when it is dropped or when
/// the process dies without unwinding.
#[derive(Debug)]
pub struct LauncherLock {
    pid_path: PathBuf,
    /// The locked file. Retained because the lock lives on this descriptor's
    /// open file description, and read only to release it.
    file: File,
}

impl LauncherLock {
    pub fn acquire(directories: &StandardDirectories) -> Result<Self, PackageError> {
        let lock = Self::acquire_at(directories.state_dir())?;
        for kind in crikey_platform::PluginKind::ALL {
            crate::native::recover_interrupted_swaps(&directories.plugin_dir(kind));
        }
        Ok(lock)
    }

    pub fn acquire_at(state_dir: &Path) -> Result<Self, PackageError> {
        fs::create_dir_all(state_dir).map_err(|error| {
            PackageError::Install(format!(
                "the launcher lock directory {} could not be created: {error}",
                state_dir.display()
            ))
        })?;
        let path = state_dir.join(LOCK_FILE);
        let pid_path = state_dir.join(PID_FILE);
        let file = match open_exclusive(&path) {
            Ok(Held::Acquired(file)) => file,
            Ok(Held::Busy) => {
                return Err(PackageError::LauncherRunning {
                    pid: read_pid(&pid_path),
                })
            }
            Err(error) => {
                return Err(PackageError::Install(format!(
                    "the launcher lock {} could not be opened: {error}",
                    path.display()
                )))
            }
        };
        if let Ok(mut pid) = OpenOptions::new().write(true).create_new(true).open(&pid_path) {
            let _ = std::io::Write::write_all(&mut pid, std::process::id().to_string().as_bytes());
        }
        Ok(Self { pid_path, file })
    }
}

impl Drop for LauncherLock {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.pid_path);
        release(&self.file);
    }
}

/// Releases the lock explicitly, rather than leaving it to the closing
/// descriptor.
///
/// Closing is *not* enough on its own, and this must not be simplified back to
/// relying on it. A `flock` lock belongs to the open file *description*, not to
/// the descriptor: `flock(2)` releases it only on an explicit `LOCK_UN` or when
/// every descriptor referring to that description has been closed. `fork`
/// duplicates every description, and a launcher holds this lock for the life of
/// the process while spawning a worker for each plugin, so from that fork until
/// the child's `exec` there is a second reference to this one. Leaving release
/// to the closing descriptor therefore keeps the lock alive for a window this
/// process can neither see nor bound, and the next acquisition reads a launcher
/// that has already released as still running. Unlocking through this
/// descriptor releases the description's lock at once, however many references
/// it has.
///
/// Windows needs no counterpart, and a symmetrical unlock there would be
/// pretending to do something: exclusivity is the handle's share mode rather
/// than an advisory lock, `CreateProcess` inherits only the handles it is told
/// to, and Rust marks this one non-inheritable, so closing the file is already
/// exact.
#[cfg(unix)]
fn release(file: &File) {
    use std::os::fd::AsRawFd;

    #[link(name = "c")]
    #[allow(unsafe_code)]
    unsafe extern "C" {
        fn flock(fd: i32, operation: i32) -> i32;
    }
    const LOCK_UN: i32 = 8;

    // The result is discarded because there is no recovery and nothing to
    // report: the only documented failures are a bad descriptor or a bad
    // operation, neither of which can happen here, and a drop cannot refuse.
    //
    // SAFETY: `file` owns a valid open descriptor for the whole call, and
    // `flock` reads only the descriptor and the flag word.
    #[allow(unsafe_code)]
    let _ = unsafe { flock(file.as_raw_fd(), LOCK_UN) };
}

/// Nothing to release: see [`release`] for why the Windows share mode makes
/// closing the handle exact.
#[cfg(not(unix))]
fn release(_file: &File) {}

enum Held {
    Acquired(File),
    Busy,
}

fn read_pid(path: &Path) -> Option<u32> {
    fs::read_to_string(path).ok()?.trim().parse().ok()
}
#[cfg(unix)]
fn open_exclusive(path: &Path) -> io::Result<Held> {
    use std::os::fd::AsRawFd;

    #[link(name = "c")]
    #[allow(unsafe_code)]
    unsafe extern "C" {
        fn flock(fd: i32, operation: i32) -> i32;
    }
    const LOCK_EX: i32 = 2;
    const LOCK_NB: i32 = 4;

    // Never truncated: the file is a lock, its contents are irrelevant, and
    // truncating it would rewrite a file another process may hold open.
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)?;
    // SAFETY: `file` owns a valid open descriptor for the whole call, and
    // `flock` reads only the descriptor and the flag word. The lock is
    // released when the descriptor is closed, which `File`'s `Drop` does.
    #[allow(unsafe_code)]
    let outcome = unsafe { flock(file.as_raw_fd(), LOCK_EX | LOCK_NB) };
    if outcome == 0 {
        return Ok(Held::Acquired(file));
    }
    let error = io::Error::last_os_error();
    if error.kind() == io::ErrorKind::WouldBlock {
        Ok(Held::Busy)
    } else {
        Err(error)
    }
}

#[cfg(windows)]
fn open_exclusive(path: &Path) -> io::Result<Held> {
    use std::os::windows::fs::OpenOptionsExt;

    /// `ERROR_SHARING_VIOLATION`: another process holds the file with no
    /// sharing, which is exactly what a live launcher looks like.
    const ERROR_SHARING_VIOLATION: i32 = 32;

    // `share_mode(0)` is the exclusive open: while this handle lives, no other
    // process can open the file at all. No extra dependency and no pid.
    match OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .share_mode(0)
        .open(path)
    {
        Ok(file) => Ok(Held::Acquired(file)),
        Err(error) if error.raw_os_error() == Some(ERROR_SHARING_VIOLATION) => Ok(Held::Busy),
        Err(error) => Err(error),
    }
}

#[cfg(not(any(unix, windows)))]
compile_error!("the launcher lock needs either flock (unix) or an exclusive share mode (windows)");

// Here rather than in `tests/`: the invariant below is about a second reference
// to the guard's own descriptor, and nothing outside this file can obtain one.
#[cfg(all(test, unix))]
mod tests {
    use super::*;

    /// Dropping the guard releases the lock even when another descriptor still
    /// refers to the same open file description.
    ///
    /// `Command::spawn` forks, and a launcher spawns a worker for every plugin
    /// while holding this lock, so that second reference exists in production
    /// for as long as it takes the child to reach `exec`. `try_clone` is `dup`,
    /// which produces exactly the same sharing without the race, so the
    /// consequence of leaving release to the closing descriptor — a lock the
    /// guard no longer owns and the next installation is refused by — is a
    /// deterministic failure here rather than an occasional one under load.
    ///
    /// Unix only because the mechanism is: Windows exclusivity is the handle's
    /// share mode, and Rust marks the handle non-inheritable, so there is no
    /// second reference to release.
    #[test]
    fn dropping_the_guard_releases_a_lock_a_duplicated_descriptor_still_refers_to() {
        let state_dir = std::env::temp_dir().join(format!("crikey-launcher-lock-{}", std::process::id()));
        let _ = fs::remove_dir_all(&state_dir);

        let lock = LauncherLock::acquire_at(&state_dir).expect("a first lock is acquired");
        let inherited = lock.file.try_clone().expect("the descriptor duplicates");
        drop(lock);

        let reacquired = LauncherLock::acquire_at(&state_dir);
        assert!(
            reacquired.is_ok(),
            "a dropped guard must leave no lock behind, got {:?}",
            reacquired.err()
        );

        drop(inherited);
        drop(reacquired);
        let _ = fs::remove_dir_all(&state_dir);
    }
}
