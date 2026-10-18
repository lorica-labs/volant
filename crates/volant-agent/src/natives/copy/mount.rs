// SPDX-License-Identifier: GPL-3.0-or-later

//! Whether a rename between two places crosses a mount, and the mount id the kernel gives.

#[cfg(target_os = "linux")]
use std::ffi::CString;

/// Whether a rename between two places, each given by its device and its mount (`None` where the
/// kernel does not say), would fail with EXDEV or EBUSY. The two can differ apart: a btrfs
/// subvolume has a device of its own on its parent's mount, and a bind mount shows its file
/// system's device on a mount of its own.
pub(super) fn crosses(a: (u64, Option<u64>), b: (u64, Option<u64>)) -> bool {
    a.0 != b.0 || a.1.is_none() || a.1 != b.1
}

/// `STATX_MNT_ID`, which the `libc` crate declares for glibc only.
#[cfg(target_os = "linux")]
const STATX_MNT_ID: libc::c_uint = 0x1000;

/// The id of the mount `path` is on (`statx`, Linux 5.8 and later), or `None` when the kernel
/// does not say.
#[cfg(target_os = "linux")]
pub(super) fn mount_id(path: &str, follow: bool) -> Option<u64> {
    /// A `struct statx`, as bytes: `stx_mask` first, `stx_mnt_id` at byte 144.
    #[repr(C, align(8))]
    struct Statx([u8; 256]);
    let path = CString::new(path).ok()?;
    let mut buf = Statx([0; 256]);
    let flags = if follow { 0 } else { libc::AT_SYMLINK_NOFOLLOW };
    // SAFETY: `buf` is larger than the `struct statx` the kernel writes.
    let rc = unsafe {
        libc::syscall(
            libc::SYS_statx,
            libc::AT_FDCWD,
            path.as_ptr(),
            flags,
            STATX_MNT_ID,
            buf.0.as_mut_ptr(),
        )
    };
    let mask = u32::from_ne_bytes(buf.0[0..4].try_into().ok()?);
    if rc != 0 || mask & STATX_MNT_ID == 0 {
        return None;
    }
    Some(u64::from_ne_bytes(buf.0[144..152].try_into().ok()?))
}

/// Elsewhere the native hands every task back before it gets here.
#[cfg(not(target_os = "linux"))]
pub(super) fn mount_id(_: &str, _: bool) -> Option<u64> {
    None
}
