// SPDX-License-Identifier: GPL-3.0-or-later
//! The variables ansible-core's `generate_ansible_template_vars` sets about a template file, on
//! top of its path: the controller's host name, the file's owner and modification time, and the
//! time of the render.

use std::path::Path;
use std::time::UNIX_EPOCH;

use serde_json::Value;

/// `template_host`, `template_uid`, `template_mtime` and `template_run_date` for `file`. The two
/// times are text in Python's `str` of a `datetime`, which is how a template prints them; they
/// are not `datetime` values.
pub(crate) fn file_vars(file: &Path) -> [(&'static str, Value); 4] {
    let meta = std::fs::metadata(file).ok();
    let mtime = meta
        .as_ref()
        .and_then(|m| m.modified().ok())
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| {
            super::clock::local_python_str(
                i64::try_from(d.as_secs()).unwrap_or(0),
                d.subsec_micros(),
            )
        });
    [
        ("template_host", Value::from(node_name())),
        ("template_uid", Value::from(owner(meta.as_ref()))),
        ("template_mtime", mtime.map_or(Value::Null, Value::from)),
        (
            "template_run_date",
            Value::from(super::clock::now_python_str()),
        ),
    ]
}

/// `platform.node()`.
#[cfg(unix)]
pub(crate) fn node_name() -> String {
    let mut buf = [0_u8; 256];
    // SAFETY: `buf` is valid for its length; `gethostname` writes a terminated name into it.
    let ok = unsafe { libc::gethostname(buf.as_mut_ptr().cast(), buf.len()) } == 0;
    let end = buf.iter().position(|b| *b == 0).unwrap_or(buf.len());
    if ok {
        String::from_utf8_lossy(&buf[..end]).into_owned()
    } else {
        String::new()
    }
}

#[cfg(not(unix))]
pub(crate) fn node_name() -> String {
    std::env::var("COMPUTERNAME").unwrap_or_default()
}

/// The name of the file's owner, else its numeric id, as the reference falls back.
#[cfg(unix)]
fn owner(meta: Option<&std::fs::Metadata>) -> String {
    use std::os::unix::fs::MetadataExt;
    let Some(uid) = meta.map(MetadataExt::uid) else {
        return String::new();
    };
    let mut entry: libc::passwd = unsafe { std::mem::zeroed() };
    let mut buf = vec![0_u8; 4096];
    let mut found: *mut libc::passwd = std::ptr::null_mut();
    // SAFETY: `entry` and `buf` outlive the call, which fills them or leaves `found` null.
    let rc = unsafe {
        libc::getpwuid_r(
            uid,
            &raw mut entry,
            buf.as_mut_ptr().cast(),
            buf.len(),
            &raw mut found,
        )
    };
    if rc != 0 || found.is_null() {
        return uid.to_string();
    }
    // SAFETY: `pw_name` points into `buf`, a terminated string `getpwuid_r` wrote.
    unsafe { std::ffi::CStr::from_ptr(entry.pw_name) }
        .to_string_lossy()
        .into_owned()
}

#[cfg(not(unix))]
fn owner(_meta: Option<&std::fs::Metadata>) -> String {
    std::env::var("USERNAME").unwrap_or_default()
}
