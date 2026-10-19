// SPDX-License-Identifier: GPL-3.0-or-later
//! The directory that holds the cached agent on a host, and the shell that checks it.
//!
//! The directory is `<remote_tmp>/volant-agent-<version>-<uid>`, with the numeric id of the user
//! the agent runs as. A host can share one `remote_tmp` between the login user and a `become`
//! user, and a user must never run a program that another user owns or can write to. A name
//! per user keeps two users from meeting in one directory, and the checks below keep a third
//! user, who made the directory first, from being trusted.
//!
//! Every command that runs the agent, or puts it in place, runs [`CHECKS`] on the host first. A
//! path is accepted only when it is not a symbolic link, belongs to the user the command runs
//! as, and has no group or other write bit. `remote_tmp` itself is accepted only when other
//! users cannot rename an entry in it: it belongs to that user, to root, or to the user whom
//! `sudo` reports in `SUDO_UID` (the login user, who can already act as root), and it is closed
//! to other writes, or has the sticky bit. A `remote_tmp` that the user owns may be open to its
//! group when that group is the user's own primary group, as a umask of 002 with a group per
//! user makes the home directory and what is under it.
//!
//! The checks use `ls -ldn`, which POSIX specifies and GNU, BusyBox and BSD `ls` all print the
//! same way, so no `stat` option that a host may lack is needed.

use super::bootstrap::EXIT_AGENT_DIR_UNSAFE;
use super::defaults::SshTarget;
use super::shell::shell_word;

/// Shell functions, defined once per command line.
///
/// `ent LINE PATH` reads the mode, the owner and the group out of one line of `ls -ldn` into `m`,
/// `o` and `gp`, and leaves them empty when the line is not about `PATH`. `ls -l` shows a link, not what it
/// points at, so a link has the mode of a link. `okmode TYPE` succeeds when that entry has the
/// type (`d` or `-`), belongs to `$u` and has no group or other write bit. `oktmp` is the test
/// for `remote_tmp`, which may also be sticky, or open to its group when it belongs to `$u` and
/// to `$u`'s own primary group and no ACL shows in the mode (`+`), because a named ACL entry is a
/// write grant to somebody else. `fit TYPE
/// PATH` runs `ls` for one path and then `okmode`. `say` names a refused path.
///
/// `verify` checks `remote_tmp`, the directory and the program with one `ls`, whose output
/// `judge` reads: `LC_ALL=C` sorts the three paths in that order, because each is a prefix of
/// the next, and `ent` fails any line that is not about the path it is given. `prepare` makes
/// the directory for an upload, with `mkdir -m 700`, because a default ACL on `remote_tmp` takes
/// the place of the umask. It accepts a directory another link of the same account made in the
/// meantime, which `fit` then judges like any other. `sweep` removes the other versions'
/// directories that belong to this user and that nobody else can write.
const CHECKS: &str = concat!(
    r#"ent() { m=; o=; gp=; case "$1" in *" $2"|*" $2 -> "*) set -f; set -- $1; set +f; m=$1; o=$3; gp=$4;; esac; }; "#,
    r#"okmode() { case "$m" in "$1"????[!w]??[!w]*) [ "$o" = "$u" ]; return;; esac; return 1; }; "#,
    r#"oktmp() { case "$m" in d*) ;; *) return 1;; esac; "#,
    r#"[ "$o" = "$u" ] || [ "$o" = 0 ] || [ "$o" = "${SUDO_UID:-x}" ] || return 1; "#,
    r#"case "$m" in d????????[tT]*) return 0;; d???????w*) return 1;; d????w????+) return 1;; d????w*) [ "$o" = "$u" ] && [ "$gp" = "$(id -g)" ]; return;; esac; return 0; }; "#,
    r#"fit() { ent "$(ls -ldn -- "$2" 2>/dev/null)" "$2"; okmode "$1"; }; "#,
    r#"say() { printf '%s\n' "$1: permissions ${m:-unknown}, owner uid ${o:-unknown}: $2" >&2; }; "#,
    r#"bad_tmp() { say "$rt" "other users could replace the agent directory in it; it must belong to uid $u, to root or to the user who ran sudo, and other users must not be able to write to it unless it has the sticky bit"; }; "#,
    r#"bad_dir() { say "$d" "it must be a real directory of uid $u with no group or other write bit"; }; "#,
    r#"bad_file() { say "$a" "it must be a regular file of uid $u with no group or other write bit"; }; "#,
    r#"judge() { ent "$1" "$rt/"; oktmp || { bad_tmp; return 1; }; ent "$2" "$d"; okmode d || { bad_dir; return 1; }; ent "$3" "$a"; okmode - || { bad_file; return 1; }; }; "#,
    r#"verify() { LC_ALL=C ls -ldn -- "$rt/" "$d" "$a" 2>/dev/null | { read -r l1; read -r l2; read -r l3; judge "$l1" "$l2" "$l3"; }; }; "#,
    r#"prepare() { [ -d "$rt/" ] || mkdir -p -m 700 "$rt" || return 1; ent "$(ls -ldn -- "$rt/" 2>/dev/null)" "$rt/"; oktmp || { bad_tmp; return @UNSAFE@; }; "#,
    r#"if [ -L "$d" ] || [ -e "$d" ]; then if ! fit d "$d"; then "#,
    r#"case "$m" in d*) [ "$o" = "$u" ] || { bad_dir; return @UNSAFE@; };; *) bad_dir; return @UNSAFE@;; esac; "#,
    r#"rm -f "$d/volant-agent" "$d"/volant-agent.tmp.* "$d/agent.stderr"; "#,
    r#"rmdir "$d" 2>/dev/null || { bad_dir; return @UNSAFE@; }; mkdir -m 700 "$d" 2>/dev/null || [ -d "$d" ] || return 1; fi; "#,
    r#"else mkdir -m 700 "$d" 2>/dev/null || [ -d "$d" ] || return 1; fi; fit d "$d" || { bad_dir; return @UNSAFE@; }; }; "#,
    r#"sweep() { for old in "$rt"/volant-agent-*; do [ "$old" = "$d" ] && continue; "#,
    r#"ent "$(ls -ldn -- "$old" 2>/dev/null)" "$old"; okmode d && rm -rf -- "$old"; done; return 0; }; "#,
);

impl SshTarget {
    /// The assignments every agent command starts with: the id of the user it runs as, `remote_tmp`
    /// (`rt`), the directory (`d`) and the program (`a`). The tilde of a `remote_tmp` is expanded
    /// by the shell that runs this, which is the target user's own.
    pub(super) fn agent_vars(&self) -> String {
        format!(
            "u=$(id -u) || exit 1; rt={}; d=\"$rt/volant-agent-{}-$u\"; a=\"$d/volant-agent\"; ",
            shell_word(&self.remote_tmp),
            env!("CARGO_PKG_VERSION")
        )
    }

    /// The functions, for a command that checks or makes the directory.
    pub(super) fn agent_checks(&self) -> String {
        format!(
            "{}{}",
            self.agent_vars(),
            CHECKS.replace("@UNSAFE@", &EXIT_AGENT_DIR_UNSAFE.to_string())
        )
    }

    /// The agent's directory for a message: the numeric id is only known on the host.
    pub(super) fn dir_hint(&self) -> String {
        format!(
            "{}/volant-agent-{}-<uid>",
            self.remote_tmp,
            env!("CARGO_PKG_VERSION")
        )
    }

    /// Starts the agent, once `remote_tmp`, the directory and the program have passed the checks
    /// again. The probe checked them earlier, in another `ssh` session, so this closes the gap
    /// of a run that took its time to upload; what is left is the time between the last check
    /// and `exec`. `redirect` is appended to the `exec` command, for the stderr of a link
    /// through `su`.
    pub(super) fn exec_command(&self, redirect: &str) -> String {
        format!(
            "{}verify || exit {EXIT_AGENT_DIR_UNSAFE}; exec \"$a\"{redirect}",
            self.agent_checks()
        )
    }
}

#[cfg(all(test, unix))]
mod tests;
