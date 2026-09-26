// SPDX-License-Identifier: GPL-3.0-or-later
//! The reasons the native `setup` hands back for a host outside its subset, as it words them.
//!
//! A test that runs the native on the machine under test cannot require an answer: a CI runner
//! can sit outside the subset (measured: GitHub's `ubuntu-latest` has `/usr/bin/rpm`). Such a
//! test asserts that a hand-back names one of these and stops; where the native answers, it
//! compares the answer. Where the subset ends is the business of the unit tests on fake roots.

const HOST_EXITS: &[&str] = &[
    "outside Debian and Ubuntu",
    "/etc/os-release is missing",
    "names another distribution",
    "reads as another distribution to the reference",
    "names neither Debian nor Ubuntu",
    "is a release file distro would read",
    "has no VERSION_ID",
    "names no codename",
    "needs distro's parser",
    "/etc/debian_version is not ASCII",
    "the interpreter imports distro",
    "/usr/bin/rpm exists",
    "SELinux is enabled",
    "is not in /etc/hosts",
    "has several addresses in /etc/hosts",
    "host names are not resolved from /etc/hosts first",
    "/etc/hosts has an address the native does not read",
    "is not a host name",
    "the password database is not read from /etc/passwd first",
    "/etc/passwd has NIS entries",
    "has no entry in /etc/passwd",
    "holds local facts",
    "/proc/1/comm is unreadable",
    "is not a public key",
    "the module's locale cannot be set",
    "lsb_release printed something other than UTF-8",
    "/etc/lsb-release has a line without '='",
];

/// Whether `reason` is a host outside the subset, rather than the native failing.
pub fn is_host_exit(reason: &str) -> bool {
    HOST_EXITS.iter().any(|exit| reason.contains(exit))
}
