//! Landlock backend: raw syscalls (no extra dependency — `libc` already
//! carries the syscall numbers). The ruleset is built in the parent (opening
//! path fds allocates, so it must happen before fork); the `pre_exec` hook
//! only calls `prctl(PR_SET_NO_NEW_PRIVS)` and `landlock_restrict_self`,
//! both async-signal-safe. Children inherit the domain, so everything the
//! agent process runs is confined too.

use std::ffi::CString;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::sync::OnceLock;

use super::policy::{Access, Rule};

const CREATE_RULESET_VERSION: u32 = 1 << 0;
const RULE_PATH_BENEATH: libc::c_int = 1;

const FS_EXECUTE: u64 = 1 << 0;
const FS_WRITE_FILE: u64 = 1 << 1;
const FS_READ_FILE: u64 = 1 << 2;
const FS_READ_DIR: u64 = 1 << 3;
const FS_REMOVE_DIR: u64 = 1 << 4;
const FS_REMOVE_FILE: u64 = 1 << 5;
const FS_MAKE_CHAR: u64 = 1 << 6;
const FS_MAKE_DIR: u64 = 1 << 7;
const FS_MAKE_REG: u64 = 1 << 8;
const FS_MAKE_SOCK: u64 = 1 << 9;
const FS_MAKE_FIFO: u64 = 1 << 10;
const FS_MAKE_BLOCK: u64 = 1 << 11;
const FS_MAKE_SYM: u64 = 1 << 12;
const FS_REFER: u64 = 1 << 13; // ABI 2
const FS_TRUNCATE: u64 = 1 << 14; // ABI 3

/// Rights that can be granted on a non-directory. Anything else on a file
/// rule is `EINVAL`.
const FILE_RIGHTS: u64 = FS_EXECUTE | FS_WRITE_FILE | FS_READ_FILE | FS_TRUNCATE;

const SCOPE_SIGNAL: u64 = 1 << 1; // ABI 6

#[repr(C)]
struct RulesetAttr {
    handled_access_fs: u64,
    handled_access_net: u64,
    scoped: u64,
}

#[repr(C, packed)]
struct PathBeneathAttr {
    allowed_access: u64,
    parent_fd: i32,
}

/// The kernel's Landlock ABI version, probed once. 0 = unavailable (kernel
/// too old, or the LSM not enabled).
pub fn abi() -> i32 {
    static ABI: OnceLock<i32> = OnceLock::new();
    *ABI.get_or_init(|| {
        // SAFETY: the documented version probe — NULL attr, size 0.
        let v = unsafe {
            libc::syscall(
                libc::SYS_landlock_create_ruleset,
                std::ptr::null::<RulesetAttr>(),
                0usize,
                CREATE_RULESET_VERSION,
            )
        };
        if v < 0 { 0 } else { v as i32 }
    })
}

fn handled_fs(abi: i32) -> u64 {
    let mut h = FS_EXECUTE
        | FS_WRITE_FILE
        | FS_READ_FILE
        | FS_READ_DIR
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
        h |= FS_REFER;
    }
    if abi >= 3 {
        h |= FS_TRUNCATE;
    }
    h
}

fn rights(access: Access, handled: u64) -> u64 {
    let r = match access {
        Access::ReadOnly => FS_EXECUTE | FS_READ_FILE | FS_READ_DIR,
        Access::ReadWrite => handled,
        Access::ListOnly => FS_READ_DIR,
        Access::Devices => FS_READ_FILE | FS_WRITE_FILE | FS_READ_DIR | FS_TRUNCATE,
    };
    r & handled
}

/// Build a Landlock ruleset fd from planned rules. Paths that vanished or
/// cannot be opened are skipped (they simply stay inaccessible).
pub fn build_ruleset(rules: &[Rule]) -> io::Result<OwnedFd> {
    let abi = abi();
    if abi < 1 {
        return Err(io::Error::other("Landlock is not available on this kernel"));
    }
    let handled = handled_fs(abi);
    let attr = RulesetAttr {
        handled_access_fs: handled,
        handled_access_net: 0,
        scoped: if abi >= 6 { SCOPE_SIGNAL } else { 0 },
    };
    // SAFETY: attr is a valid, fully initialised struct of the given size;
    // older kernels accept the larger size because the trailing fields they
    // don't know are zero.
    let fd = unsafe {
        libc::syscall(
            libc::SYS_landlock_create_ruleset,
            &attr as *const RulesetAttr,
            std::mem::size_of::<RulesetAttr>(),
            0u32,
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: the syscall returned a fresh fd we now own.
    let ruleset = unsafe { OwnedFd::from_raw_fd(fd as i32) };
    for rule in rules {
        let Ok(cpath) = CString::new(rule.path.as_os_str().as_bytes()) else {
            continue;
        };
        // SAFETY: valid NUL-terminated path; O_PATH opens without access.
        let pfd = unsafe { libc::open(cpath.as_ptr(), libc::O_PATH | libc::O_CLOEXEC) };
        if pfd < 0 {
            continue;
        }
        // SAFETY: fresh fd from open(2).
        let pfd = unsafe { OwnedFd::from_raw_fd(pfd) };
        // SAFETY: zeroed stat is a valid out-param.
        let mut st: libc::stat = unsafe { std::mem::zeroed() };
        // SAFETY: valid fd and out-pointer.
        if unsafe { libc::fstat(pfd.as_raw_fd(), &mut st) } != 0 {
            continue;
        }
        let is_dir = st.st_mode & libc::S_IFMT == libc::S_IFDIR;
        let mut allowed = rights(rule.access, handled);
        if !is_dir {
            allowed &= FILE_RIGHTS;
        }
        if allowed == 0 {
            continue;
        }
        let pb = PathBeneathAttr {
            allowed_access: allowed,
            parent_fd: pfd.as_raw_fd(),
        };
        // SAFETY: valid ruleset fd, rule type and attr pointer.
        let rc = unsafe {
            libc::syscall(
                libc::SYS_landlock_add_rule,
                ruleset.as_raw_fd(),
                RULE_PATH_BENEATH,
                &pb as *const PathBeneathAttr,
                0u32,
            )
        };
        if rc != 0 {
            tracing::debug!(
                path = %rule.path.display(),
                "landlock_add_rule failed: {}",
                io::Error::last_os_error()
            );
        }
    }
    Ok(ruleset)
}

/// Runs in the forked child, before exec: no allocation, only syscalls.
pub fn restrict_self(ruleset_fd: i32) -> io::Result<()> {
    // SAFETY: plain prctl/syscall in the child between fork and exec.
    unsafe {
        if libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) != 0 {
            return Err(io::Error::last_os_error());
        }
        if libc::syscall(libc::SYS_landlock_restrict_self, ruleset_fd, 0u32) != 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}
