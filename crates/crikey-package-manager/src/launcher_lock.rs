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

/// File holding the lock holder's pid, for diagnostics only.
const PID_FILE: &str = "launcher.pid";

/// An acquired exclusive launcher lock.
///
/// Held for as long as the value lives; the operating system releases it when
/// the file is closed, including when the process dies without unwinding.
#[derive(Debug)]
pub struct LauncherLock {
    pid_path: PathBuf,
    /// Retained because closing the file is what releases the lock. Never read.
    _file: File,
}

impl LauncherLock {
    /// Acquires the launcher lock for this process.
    ///
    /// `crikey run` calls this at startup and keeps the value alive for the
    /// life of the launcher. Installation acquires the same lock, so exactly
    /// one of the two can be underway at a time.
    pub fn acquire(directories: &StandardDirectories) -> Result<Self, PackageError> {
        Self::acquire_at(directories.state_dir())
    }

    /// Acquires the lock in an explicit state directory.
    ///
    /// Separate from [`Self::acquire`] so a test can state the directory it
    /// means rather than arranging the process environment, which no parallel
    /// test can do safely.
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

        // Best effort: a missing pid costs a less specific message, never
        // correctness, so it must not fail an acquisition that succeeded.
        let _ = fs::write(&pid_path, std::process::id().to_string());

        Ok(Self {
            pid_path,
            _file: file,
        })
    }
}

impl Drop for LauncherLock {
    fn drop(&mut self) {
        // The lock itself is released by closing the file. Only the diagnostic
        // pid is cleaned up here, and only so a later refusal cannot quote a
        // process that has already exited.
        let _ = fs::remove_file(&self.pid_path);
    }
}

/// Whether an exclusive open succeeded or found the file already locked.
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
