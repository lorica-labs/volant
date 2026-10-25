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
//! `sudo` reports in `SUDO_UID` or [`AgentCommands::login_tmp`] in `VOLANT_LOGIN_UID` (the login
//! user, who can already act as the escalated one), and it is closed
//! to other writes, or has the sticky bit. A `remote_tmp` that the user owns may be open to its
//! group when that group is the user's own primary group, as a umask of 002 with a group per
//! user makes the home directory and what is under it.
//!
//! The checks use `ls -ldn`, which POSIX specifies and GNU, BusyBox and BSD `ls` all print the
//! same way, so no `stat` option that a host may lack is needed.

use super::bootstrap::{
    EXIT_AGENT_DIR_UNSAFE, EXIT_AGENT_MISSING, EXIT_AGENT_UNRUNNABLE, EXIT_NO_SPACE,
    EXIT_NOT_FUSED, EXIT_SHORT_WRITE, EXIT_UNAME_FAILED, space_needed_kib,
};
use super::reach::Reach;
use super::shell::shell_word;
use std::fmt::Write as _;

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
    r#"[ "$o" = "$u" ] || [ "$o" = 0 ] || [ "$o" = "${SUDO_UID:-${VOLANT_LOGIN_UID:-x}}" ] || return 1; "#,
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

/// The shell commands that check, fill and start the agent's directory, for any target the
/// bootstrap reaches.
/// Runs the agent and exits with its status. Not `exec`: a program a signal stops makes `ssh`
/// exit 255, as a lost connection does, where a shell that waits for it exits 128 and the
/// signal, as the reference's own shell does for a module that dies. The shell's report of the
/// signal goes nowhere; the agent keeps the command's stderr.
fn run_agent(redirect: &str) -> String {
    format!("exec 3>&2 2>/dev/null; \"$a\" 2>&3 3>&-{redirect}; exit $?")
}

pub(super) trait AgentCommands: Reach {
    /// The assignments every agent command starts with: the id of the user it runs as, `remote_tmp`
    /// (`rt`), the directory (`d`) and the program (`a`). The tilde of a `remote_tmp` is expanded
    /// by the shell that runs this, which is the target user's own.
    fn agent_vars(&self) -> String {
        format!(
            "u=$(id -u) || exit 1; rt={}; d=\"$rt/volant-agent-{}-$u\"; a=\"$d/volant-agent\"; ",
            shell_word(self.remote_tmp()),
            env!("CARGO_PKG_VERSION")
        )
    }

    /// What an escalated command runs as the login user first: an absolute `remote_tmp` that
    /// does not exist yet is made by the login user, mode 0700, as ansible-core 2.19.12 makes it
    /// for a task with `become`, so the escalated link and the plain one can both use it. The
    /// login user's id travels to the checks for a `su`, which, unlike `sudo`, says nothing
    /// about who started it. A `remote_tmp` under `~` names each user's own home and is left to
    /// that user.
    fn login_tmp(&self) -> String {
        let rt = self.remote_tmp();
        let made = if rt.starts_with('/') {
            let rt = shell_word(rt);
            format!("[ -d {rt}/ ] || (umask 077 && mkdir -p {rt}) 2>/dev/null; ")
        } else {
            String::new()
        };
        format!("{made}VOLANT_LOGIN_UID=$(id -u); export VOLANT_LOGIN_UID; ")
    }

    /// The functions, for a command that checks or makes the directory.
    fn agent_checks(&self) -> String {
        format!(
            "{}{}",
            self.agent_vars(),
            CHECKS.replace("@UNSAFE@", &EXIT_AGENT_DIR_UNSAFE.to_string())
        )
    }

    /// The agent's directory for a message: the numeric id is only known on the host.
    fn dir_hint(&self) -> String {
        format!(
            "{}/volant-agent-{}-<uid>",
            self.remote_tmp(),
            env!("CARGO_PKG_VERSION")
        )
    }

    /// The probe and the start in one command: the agent is started when the directory passes
    /// the checks and the cached program answers `--version --build-id` with the line `expected`
    /// gives for this machine's `uname -m` pattern; otherwise it exits [`EXIT_NOT_FUSED`] having
    /// written nothing on stdout.
    fn fused_command(&self, expected: &[(&str, String)]) -> String {
        let mut cases = String::new();
        for (arch, line) in expected {
            let _ = write!(cases, "{arch}) w={};; ", shell_word(line));
        }
        format!(
            "{}case \"$(uname -m)\" in {cases}*) exit {EXIT_NOT_FUSED};; esac;              [ -x \"$a\" ] && verify && [ \"$(\"$a\" --version --build-id 2>/dev/null)\" = \"$w\" ]              && {{ {}; }}; exit {EXIT_NOT_FUSED}",
            self.agent_checks(),
            run_agent("")
        )
    }

    /// Starts the agent, once `remote_tmp`, the directory and the program have passed the checks
    /// again. The probe checked them earlier, in another `ssh` session, so this closes the gap
    /// of a run that took its time to upload; what is left is the time between the last check
    /// and the start. `redirect` is appended to the agent's command, for the stderr of a link
    /// through `su`.
    fn exec_command(&self, redirect: &str) -> String {
        format!(
            "{}verify || exit {EXIT_AGENT_DIR_UNSAFE}; {}",
            self.agent_checks(),
            run_agent(redirect)
        )
    }

    /// Prints the machine architecture, then the cached agent's version and the hash of its own
    /// file, or exits 42 when no agent is cached. The architecture names the binary this
    /// controller would upload, whose hash decides whether the cached one is reused. A cached
    /// agent that cannot run exits 45 and `uname` failing exits 46, so neither hands its own
    /// status back to `ssh`, where 255 would read as a connection failure. A cached agent in a
    /// directory, or under a `remote_tmp`, that another user could change is never run: the probe
    /// exits 47 and names the path on stderr.
    fn probe_command(&self) -> String {
        format!(
            "uname -m || exit {EXIT_UNAME_FAILED}; {checks}if [ -x \"$a\" ]; then verify || exit {EXIT_AGENT_DIR_UNSAFE}; \"$a\" --version --build-id || {{ echo \"$a: exit status $?\" >&2; exit {EXIT_AGENT_UNRUNNABLE}; }}; else exit {EXIT_AGENT_MISSING}; fi",
            checks = self.agent_checks()
        )
    }

    /// Reads the agent from stdin into the cache, atomically, after checking free space. When `df`
    /// cannot say, as in a chroot with no `/proc`, the check is left out, and a disk that fills up
    /// is still caught by the byte count. It
    /// reads the byte count it was told rather than up to the end of its input, because behind
    /// `su` that input is a terminal, where no end ever comes; `head -c` has that on every host
    /// an agent is built for (GNU coreutils, BusyBox and toybox alike). The byte count is
    /// verified before the temporary copy is made executable and renamed, so a
    /// transfer cut short never lands under the final name; older cached versions are removed
    /// afterwards, and failing to remove one does not fail an upload that has already landed.
    /// The temporary name carries the remote shell's pid, so two uploads racing on one host
    /// write to separate files instead of interleaving into one of exactly the right size. A
    /// connection killed mid-transfer still orphans its own `.tmp.<pid>` file, since nothing
    /// left running can clean it up; an hour-old sweep after a successful `mv` removes those
    /// without ever touching a sibling upload still in flight.
    ///
    /// Everything it makes is made under `umask 077`. It first checks `remote_tmp` and the
    /// agent's directory, and exits 47 with the reason on stderr when another user owns or can
    /// write to either; a directory that is this user's own but too open is emptied of what this
    /// controller put there and made again. Only the other versions' directories that this user
    /// owns are removed.
    fn upload_command(&self, size: u64) -> String {
        format!(
            "umask 077; {checks}prepare || exit $?; \
             t=\"$d/volant-agent.tmp.$$\"; \
             avail=$(df -Pk \"$d\" | awk 'NR==2 {{print $4}}'); \
             case \"$avail\" in ''|*[!0-9]*) avail={need};; esac; \
             [ \"$avail\" -ge {need} ] || {{ echo \"$avail\"; exit {EXIT_NO_SPACE}; }}; \
             head -c {size} > \"$t\" || {{ rm -f \"$t\"; exit 1; }}; \
             [ \"$(wc -c < \"$t\")\" -eq {size} ] || {{ rm -f \"$t\"; exit {EXIT_SHORT_WRITE}; }}; \
             chmod 755 \"$t\" || {{ rm -f \"$t\"; exit 1; }}; \
             mv \"$t\" \"$a\" || {{ rm -f \"$t\"; exit 1; }}; \
             find \"$d\" -name 'volant-agent.tmp.*' -mmin +60 -delete 2>/dev/null; \
             sweep; \
             exit 0",
            checks = self.agent_checks(),
            need = space_needed_kib(size),
        )
    }
}

impl<T: Reach + ?Sized> AgentCommands for T {}

#[cfg(all(test, unix))]
mod tests;
