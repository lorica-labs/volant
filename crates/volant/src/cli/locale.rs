// SPDX-License-Identifier: GPL-3.0-or-later
//! The locale check every ansible-core program makes before it reads its arguments.

use std::ffi::CStr;

/// Exits as ansible-core 2.19.12's `initialize_locale` does when the environment's locale cannot
/// be set or does not encode UTF-8, measured on `ansible --version`. With `LC_ALL` unset, Python
/// coerces a C or POSIX locale to C.UTF-8 and the program runs; `LC_ALL=C` is refused.
pub fn check_locale() {
    // SAFETY: called while the program parses its arguments, before any other thread exists.
    if unsafe { libc::setlocale(libc::LC_ALL, c"".as_ptr()) }.is_null() {
        refuse("Ansible could not initialize the preferred locale: unsupported locale setting");
    }
    // SAFETY: a null locale only queries, and both calls answer a string libc owns.
    let (name, codeset) = unsafe {
        (
            CStr::from_ptr(libc::setlocale(libc::LC_CTYPE, std::ptr::null())),
            CStr::from_ptr(libc::nl_langinfo(libc::CODESET)),
        )
    };
    let detected = if matches!(name.to_bytes(), b"C" | b"POSIX") {
        if std::env::var_os("LC_ALL").is_none_or(|all| all.is_empty()) {
            return;
        }
        "None".to_string()
    } else {
        codeset.to_string_lossy().into_owned()
    };
    if !detected.eq_ignore_ascii_case("utf-8") && !detected.eq_ignore_ascii_case("utf8") {
        refuse(&format!(
            "Ansible requires the locale encoding to be UTF-8; Detected {detected}."
        ));
    }
}

fn refuse(msg: &str) -> ! {
    eprintln!("ERROR: {msg}");
    std::process::exit(1);
}
