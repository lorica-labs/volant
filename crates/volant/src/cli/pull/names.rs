// SPDX-License-Identifier: GPL-3.0-or-later
//! The machine's names, which pick the destination, the limit and the playbook.

/// The machine's names: `socket.getfqdn()`, and every name the limit carries, in order.
pub(super) struct Names {
    pub fqdn: String,
    pub all: Vec<String>,
}

pub(super) fn host_names() -> Names {
    let node = node_name();
    let fqdn = fqdn(&node);
    let mut all: Vec<String> = Vec::new();
    for name in [
        fqdn.clone(),
        node.clone(),
        first_label(&fqdn),
        first_label(&node),
    ] {
        if !all.contains(&name) {
            all.push(name);
        }
    }
    Names { fqdn, all }
}

pub(super) fn first_label(name: &str) -> String {
    name.split('.').next().unwrap_or_default().to_string()
}

/// `platform.node()`.
#[cfg(unix)]
fn node_name() -> String {
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
fn node_name() -> String {
    std::env::var("COMPUTERNAME").unwrap_or_default()
}

/// `socket.getfqdn()`: the name the node's first address resolves back to, else the node name.
/// Python takes the first of that name and its aliases holding a dot; the aliases are not read
/// here.
#[cfg(unix)]
fn fqdn(node: &str) -> String {
    use std::ffi::{CStr, CString};
    let Ok(name) = CString::new(node) else {
        return node.to_string();
    };
    let mut found: *mut libc::addrinfo = std::ptr::null_mut();
    // SAFETY: `name` is a terminated string, the hints may be null, and `found` is freed below.
    if unsafe {
        libc::getaddrinfo(
            name.as_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            &raw mut found,
        )
    } != 0
        || found.is_null()
    {
        return node.to_string();
    }
    let mut host = [0 as libc::c_char; 1025];
    // SAFETY: `found` is the list `getaddrinfo` returned; its first entry's address is read.
    let resolved = unsafe {
        let first = &*found;
        let rc = libc::getnameinfo(
            first.ai_addr,
            first.ai_addrlen,
            host.as_mut_ptr(),
            libc::socklen_t::try_from(host.len()).unwrap_or(0),
            std::ptr::null_mut(),
            0,
            libc::NI_NAMEREQD,
        );
        libc::freeaddrinfo(found);
        (rc == 0).then(|| CStr::from_ptr(host.as_ptr()).to_string_lossy().into_owned())
    };
    resolved.unwrap_or_else(|| node.to_string())
}

#[cfg(not(unix))]
fn fqdn(node: &str) -> String {
    node.to_string()
}
