//! The Linux half: Landlock rule-set construction, and the two syscalls the
//! child makes between `fork` and `exec`.
//!
//! The syscalls are declared here rather than pulled from a crate for the same
//! reason `setrlimit` is in `crikey-native-host`: this is the ABI the whole
//! confinement claim rests on, and a reader auditing that claim should find it
//! spelled out in one file.
//!
//! # Why the rule set is built in the parent
//!
//! Between `fork` and `exec` only async-signal-safe work is legal: no
//! allocation, no locks, no Rust I/O. Building the rule set opens a descriptor
//! per allowed path and allocates. So the parent builds it, and the child runs
//! exactly two syscalls on the finished descriptor.

#![allow(unsafe_code)]

use std::ffi::CString;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::raw::{c_char, c_int, c_long, c_uint};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::{Enforcement, PreparedSandbox, SandboxPolicy, SandboxReport};

const SYS_LANDLOCK_CREATE_RULESET: c_long = 444;
const SYS_LANDLOCK_ADD_RULE: c_long = 445;
const SYS_LANDLOCK_RESTRICT_SELF: c_long = 446;

/// Asks `landlock_create_ruleset` for the ABI level instead of a rule set.
const LANDLOCK_CREATE_RULESET_VERSION: u32 = 1;
const LANDLOCK_RULE_PATH_BENEATH: c_uint = 1;

const PR_SET_NO_NEW_PRIVS: c_int = 38;

/// `O_PATH` opens a descriptor that names a file without granting access to
/// its contents, which is exactly what a rule needs — and it is also why the
/// rule set can name `/dev/tty` without the open having a side effect.
const O_PATH: c_int = 0x0020_0000;
const O_CLOEXEC: c_int = 0x0008_0000;

// Filesystem access rights, Landlock ABI v1 unless noted.
const FS_WRITE_FILE: u64 = 1 << 1;
const FS_REMOVE_DIR: u64 = 1 << 4;
const FS_REMOVE_FILE: u64 = 1 << 5;
const FS_MAKE_CHAR: u64 = 1 << 6;
const FS_MAKE_DIR: u64 = 1 << 7;
const FS_MAKE_REG: u64 = 1 << 8;
const FS_MAKE_SOCK: u64 = 1 << 9;
const FS_MAKE_FIFO: u64 = 1 << 10;
const FS_MAKE_BLOCK: u64 = 1 << 11;
const FS_MAKE_SYM: u64 = 1 << 12;
/// ABI v2: renaming or hard-linking across directories.
const FS_REFER: u64 = 1 << 13;
/// ABI v3: `truncate(2)` and `O_TRUNC`.
const FS_TRUNCATE: u64 = 1 << 14;

// Network access rights, Landlock ABI v4.
const NET_BIND_TCP: u64 = 1 << 0;
const NET_CONNECT_TCP: u64 = 1 << 1;

/// The rights the kernel permits on a rule whose target is not a directory.
///
/// `security/landlock/syscalls.c` rejects a rule on a regular file that names
/// any directory-only right, so a file rule must be narrowed to this set or
/// the whole `landlock_add_rule` call fails with `EINVAL`.
const FILE_ONLY_RIGHTS: u64 = FS_WRITE_FILE | FS_TRUNCATE;

#[repr(C)]
struct RulesetAttr {
    handled_access_fs: u64,
    handled_access_net: u64,
}

#[repr(C, packed)]
struct PathBeneathAttr {
    allowed_access: u64,
    parent_fd: i32,
}

#[link(name = "c")]
unsafe extern "C" {
    fn syscall(number: c_long, ...) -> c_long;
    fn prctl(option: c_int, ...) -> c_int;
    fn open(path: *const c_char, flags: c_int, ...) -> c_int;
}

/// The Landlock ABI level this kernel implements, or `None` when it has none.
///
/// A kernel built without Landlock answers `ENOSYS`; one that has it disabled
/// at boot answers `EOPNOTSUPP`. Both mean the same thing to a caller, and
/// neither is an error worth propagating: the confinement is simply not
/// available and the report says so.
fn abi_version() -> Result<u32, io::Error> {
    // SAFETY: the version query takes a null attribute pointer and a zero
    // size by definition, and returns a small integer or a negative errno.
    let result = unsafe {
        syscall(
            SYS_LANDLOCK_CREATE_RULESET,
            std::ptr::null::<RulesetAttr>(),
            0usize,
            LANDLOCK_CREATE_RULESET_VERSION,
        )
    };
    if result < 0 {
        return Err(io::Error::last_os_error());
    }
    u32::try_from(result).map_err(|_| io::Error::other("landlock reported an implausible ABI level"))
}

/// Everything a child may do to a directory it is allowed to write in.
fn directory_rights(abi: u32) -> u64 {
    let mut rights = FS_WRITE_FILE
        | FS_REMOVE_DIR
        | FS_REMOVE_FILE
        | FS_MAKE_CHAR
        | FS_MAKE_DIR
        | FS_MAKE_REG
        | FS_MAKE_SOCK
        | FS_MAKE_FIFO
        | FS_MAKE_BLOCK
        | FS_MAKE_SYM;
    if abi >= 2 {
        rights |= FS_REFER;
    }
    if abi >= 3 {
        rights |= FS_TRUNCATE;
    }
    rights
}

/// Opens `path` as a rule target without granting access to its contents.
fn open_path(path: &Path) -> io::Result<OwnedFd> {
    let raw = CString::new(path.as_os_str().as_bytes())
        .map_err(|_| io::Error::other("a sandbox path contains an interior NUL"))?;
    // SAFETY: `raw` is a valid NUL-terminated C string for the duration of the
    // call, and `O_PATH` takes no mode argument.
    let descriptor = unsafe { open(raw.as_ptr(), O_PATH | O_CLOEXEC) };
    if descriptor < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `open` returned a fresh descriptor this process now owns.
    Ok(unsafe { OwnedFd::from_raw_fd(descriptor) })
}

/// Adds one `path_beneath` rule to `ruleset`.
fn add_path_rule(ruleset: RawFd, path: &Path, abi: u32) -> io::Result<()> {
    let descriptor = open_path(path)?;
    let allowed = if path.is_dir() {
        directory_rights(abi)
    } else {
        directory_rights(abi) & FILE_ONLY_RIGHTS
    };
    let attribute = PathBeneathAttr {
        allowed_access: allowed,
        parent_fd: descriptor.as_raw_fd(),
    };
    // SAFETY: the attribute is a valid, fully initialised value of the layout
    // the kernel documents for `LANDLOCK_RULE_PATH_BENEATH`, and the
    // descriptor it names is open for the duration of the call.
    let result = unsafe {
        syscall(
            SYS_LANDLOCK_ADD_RULE,
            ruleset,
            LANDLOCK_RULE_PATH_BENEATH,
            std::ptr::from_ref(&attribute),
            0u32,
        )
    };
    if result != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Builds the rule set the policy describes.
pub(crate) fn prepare(policy: &SandboxPolicy) -> PreparedSandbox {
    let abi = match abi_version() {
        Ok(abi) => abi,
        Err(error) => {
            return PreparedSandbox {
                report: SandboxReport::unconfined(format!(
                    "this kernel provides no Landlock confinement ({error})"
                )),
                ruleset: None,
            };
        }
    };

    let wants_tcp_denial = policy.denies_tcp();
    let tcp_supported = wants_tcp_denial && abi >= 4;
    let attribute = RulesetAttr {
        handled_access_fs: directory_rights(abi),
        handled_access_net: if tcp_supported {
            NET_BIND_TCP | NET_CONNECT_TCP
        } else {
            0
        },
    };
    // Older kernels reject a structure larger than the ABI they implement, so
    // the network field is only presented when it will be understood.
    let attribute_size = if tcp_supported {
        std::mem::size_of::<RulesetAttr>()
    } else {
        std::mem::size_of::<u64>()
    };
    // SAFETY: `attribute` outlives the call and `attribute_size` never exceeds
    // its real size, so the kernel reads initialised memory only.
    let created = unsafe {
        syscall(
            SYS_LANDLOCK_CREATE_RULESET,
            std::ptr::from_ref(&attribute),
            attribute_size,
            0u32,
        )
    };
    if created < 0 {
        let error = io::Error::last_os_error();
        return PreparedSandbox {
            report: SandboxReport::unconfined(format!(
                "the Landlock rule set could not be created ({error})"
            )),
            ruleset: None,
        };
    }
    // SAFETY: the syscall returned a fresh descriptor this process now owns.
    let ruleset = unsafe { OwnedFd::from_raw_fd(created as RawFd) };

    let mut granted: Vec<PathBuf> = Vec::new();
    let mut skipped: Vec<PathBuf> = Vec::new();
    for path in policy.writable() {
        if !path.exists() {
            skipped.push(path.clone());
            continue;
        }
        match add_path_rule(ruleset.as_raw_fd(), path, abi) {
            Ok(()) => granted.push(path.clone()),
            Err(error) => {
                // A path the kernel refuses to govern must not be reported as
                // writable, and must not silently disappear either: a plugin
                // that cannot write where the host promised it could is a
                // support question, and this is the answer to it.
                return PreparedSandbox {
                    report: SandboxReport::unconfined(format!(
                        "the Landlock rule for {} was refused ({error})",
                        path.display()
                    )),
                    ruleset: None,
                };
            }
        }
    }

    let tcp_network = match (wants_tcp_denial, tcp_supported) {
        (false, _) => Enforcement::NotRequested,
        (true, true) => Enforcement::Enforced {
            mechanism: "landlock",
            abi,
        },
        (true, false) => Enforcement::Unavailable(format!(
            "this kernel implements Landlock ABI {abi}; TCP restriction needs ABI 4"
        )),
    };

    PreparedSandbox {
        report: SandboxReport {
            filesystem_write: Enforcement::Enforced {
                mechanism: "landlock",
                abi,
            },
            tcp_network,
            writable: granted,
            skipped,
        },
        ruleset: Some(ruleset),
    }
}

/// Arranges for the child of `command` to restrict itself before `exec`.
pub(crate) fn install(ruleset: Option<&OwnedFd>, command: &mut Command) {
    let Some(ruleset) = ruleset else {
        return;
    };
    match ruleset.try_clone() {
        Ok(owned) => {
            // SAFETY: `pre_exec` runs in the child between fork and exec. The
            // closure calls `prctl` and one `syscall`, both async-signal-safe,
            // on a descriptor that was opened before the fork. It allocates
            // nothing, takes no lock, and performs no Rust I/O.
            unsafe {
                command.pre_exec(move || restrict_self(owned.as_raw_fd()));
            }
        }
        Err(_) => {
            // Duplicating a descriptor only fails when the process is out of
            // them. Failing the spawn is the only correct answer: the
            // alternative is a plugin that starts unconfined while the report
            // says it did not.
            //
            // SAFETY: as above; this closure performs no work at all.
            unsafe {
                command.pre_exec(|| Err(io::Error::from(io::ErrorKind::PermissionDenied)));
            }
        }
    }
}

/// The child half: two syscalls, no allocation.
fn restrict_self(ruleset: RawFd) -> io::Result<()> {
    // SAFETY: `prctl` with `PR_SET_NO_NEW_PRIVS` takes four ignored arguments
    // and touches no memory.
    if unsafe { prctl(PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) } != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: the descriptor was created by `landlock_create_ruleset` in the
    // parent and inherited across the fork; the call reads no user memory.
    if unsafe { syscall(SYS_LANDLOCK_RESTRICT_SELF, ruleset, 0u32) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The kernel must answer the version query, or every other test in this
    /// module is measuring nothing.
    #[test]
    fn the_kernel_reports_a_landlock_abi() {
        let abi = abi_version().expect("this kernel provides Landlock");
        assert!(abi >= 1, "an ABI level below 1 is not a Landlock kernel");
    }

    /// A rule on a regular file may only carry file rights: the kernel rejects
    /// a directory right with `EINVAL`, which would take the whole rule set
    /// down rather than that one path.
    #[test]
    fn a_file_rule_is_narrowed_to_file_rights() {
        let abi = abi_version().expect("this kernel provides Landlock");
        let attribute = RulesetAttr {
            handled_access_fs: directory_rights(abi),
            handled_access_net: 0,
        };
        // SAFETY: as in `prepare`.
        let created = unsafe {
            syscall(
                SYS_LANDLOCK_CREATE_RULESET,
                std::ptr::from_ref(&attribute),
                std::mem::size_of::<u64>(),
                0u32,
            )
        };
        assert!(created >= 0, "the rule set could not be created");
        // SAFETY: the syscall returned a fresh descriptor this test now owns;
        // wrapping it closes it when the test ends, including on panic.
        let ruleset = unsafe { OwnedFd::from_raw_fd(created as RawFd) };
        let file = Path::new("/dev/null");
        assert!(file.exists(), "/dev/null is expected on any Linux host");
        add_path_rule(ruleset.as_raw_fd(), file, abi).expect("a narrowed file rule is accepted");
    }
}
