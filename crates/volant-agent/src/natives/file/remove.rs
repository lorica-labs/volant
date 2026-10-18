// SPDX-License-Identifier: GPL-3.0-or-later

//! The check before a tree is removed: whether `rmtree` could only fail where the native's message
//! names what the reference's does.

#[cfg(target_os = "linux")]
use std::fs;

use crate::natives::runtime::{Clock, Stop as Halt};

/// Hands back unless removing the tree at `top` can only fail where the reference's message
/// names what the native's does: every directory listable, writable and searchable; no mount
/// point under `top` (another device, or a mount root on the same one); no entry marked
/// immutable or append-only; and, for an agent that is not root, no sticky directory holding
/// another account's entry in a directory that account does not own.
///
/// Every entry is read with one `statx` that does not follow links and opens nothing; the clock
/// is looked at before each one. A file system that does not report the `i` and `a` attributes
/// through `statx` is taken to have none.
#[cfg(target_os = "linux")]
pub(super) fn removable(top: &str, clock: Clock) -> Result<(), Halt> {
    let meta = statx(top)?;
    walk(top, &meta, &meta, unsafe { libc::geteuid() }, clock)
}

#[cfg(not(target_os = "linux"))]
pub(super) fn removable(_: &str, _: Clock) -> Result<(), Halt> {
    Err(Halt::HandBack(
        "the removal check reads Linux attributes".into(),
    ))
}

#[cfg(target_os = "linux")]
fn walk(dir: &str, meta: &Statx, top: &Statx, euid: u32, clock: Clock) -> Result<(), Halt> {
    let back = |why: String| Err(Halt::HandBack(why));
    if !crate::natives::common::access(dir, libc::R_OK | libc::W_OK | libc::X_OK) {
        return back(format!("{dir} cannot be emptied"));
    }
    if meta.protected() {
        return back(format!("{dir} is immutable or append-only"));
    }
    let sticky = u32::from(meta.stx_mode) & 0o1000 != 0 && euid != 0 && meta.stx_uid != euid;
    let entries = fs::read_dir(dir).map_err(|err| Halt::HandBack(err.to_string()))?;
    for entry in entries {
        crate::natives::common::check(clock)?;
        let entry = entry.map_err(|err| Halt::HandBack(err.to_string()))?;
        let path = entry.path();
        let path = path
            .to_str()
            .ok_or_else(|| Halt::HandBack("a name is not UTF-8".into()))?;
        let child = statx(path)?;
        if sticky && child.stx_uid != euid {
            return back(format!(
                "{path} belongs to another account in a sticky directory"
            ));
        }
        if child.protected() {
            return back(format!("{path} is immutable or append-only"));
        }
        if u32::from(child.stx_mode) & 0o170_000 == 0o040_000 {
            if (child.stx_dev_major, child.stx_dev_minor) != (top.stx_dev_major, top.stx_dev_minor)
                || child.has(STATX_ATTR_MOUNT_ROOT)
            {
                return back(format!("{path} is a mount point"));
            }
            walk(path, &child, top, euid, clock)?;
        }
    }
    Ok(())
}

#[cfg(target_os = "linux")]
const STATX_ATTR_IMMUTABLE: u64 = 0x10;
#[cfg(target_os = "linux")]
const STATX_ATTR_APPEND: u64 = 0x20;
#[cfg(target_os = "linux")]
const STATX_ATTR_MOUNT_ROOT: u64 = 0x2000;

/// The kernel's `struct statx`, up to the fields read here: `libc` only declares it for glibc
/// and a newer musl than the agent's target.
#[cfg(target_os = "linux")]
#[repr(C)]
struct Statx {
    stx_mask: u32,
    stx_blksize: u32,
    stx_attributes: u64,
    stx_nlink: u32,
    stx_uid: u32,
    stx_gid: u32,
    stx_mode: u16,
    pad: u16,
    stx_ino: u64,
    stx_size: u64,
    stx_blocks: u64,
    stx_attributes_mask: u64,
    times: [u64; 8],
    stx_rdev_major: u32,
    stx_rdev_minor: u32,
    stx_dev_major: u32,
    stx_dev_minor: u32,
    spare: [u64; 14],
}

#[cfg(target_os = "linux")]
impl Statx {
    /// An attribute the file system reports and the entry carries.
    fn has(&self, attribute: u64) -> bool {
        self.stx_attributes_mask & self.stx_attributes & attribute != 0
    }

    fn protected(&self) -> bool {
        self.has(STATX_ATTR_IMMUTABLE) || self.has(STATX_ATTR_APPEND)
    }
}

/// `statx(path, AT_SYMLINK_NOFOLLOW, STATX_BASIC_STATS)`.
#[cfg(target_os = "linux")]
fn statx(path: &str) -> Result<Statx, Halt> {
    let name = std::ffi::CString::new(path).map_err(|err| Halt::HandBack(err.to_string()))?;
    // SAFETY: an all-zero `Statx` is a valid value of a plain integer struct.
    let mut buf: Statx = unsafe { std::mem::zeroed() };
    // SAFETY: `name` is NUL-terminated and `buf` is a 256-byte `struct statx` the call fills.
    let done = unsafe {
        libc::syscall(
            libc::SYS_statx,
            libc::AT_FDCWD,
            name.as_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
            0x7ff_u32,
            &raw mut buf,
        )
    };
    if done != 0 {
        return Err(Halt::HandBack(format!(
            "statx {path}: {}",
            std::io::Error::last_os_error()
        )));
    }
    Ok(buf)
}
