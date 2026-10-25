// SPDX-License-Identifier: GPL-3.0-or-later
//! The agent directory on a shared `remote_tmp`: who may own it, and what a planted one does.

use std::io::Write;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use rstest::rstest;
use serde_json::json;

use crate::inventory::Host;
use crate::testing::{defaults, scratch};
use crate::transport::{SshTarget, Transport};

use super::super::bootstrap::{Captured, is_this_agent};
use super::AgentCommands;

const VERSION: &str = env!("CARGO_PKG_VERSION");
/// The uid the tests hand a directory to, as if another user on the host had made it.
const OTHER_UID: u32 = 4242;
const EXIT_UNSAFE: i32 = 47;
const EXIT_MISSING: i32 = 42;

/// A host as the bootstrap sees it, rooted in a scratch directory the test owns. The shell
/// commands run on this machine, so a file the test makes is a file "on the host".
struct Remote {
    root: tempfile::TempDir,
    target: SshTarget,
    handed_over: bool,
}

impl Remote {
    fn new() -> Self {
        let root = scratch();
        let shared = root.path().join("shared");
        std::fs::create_dir(&shared).unwrap();
        std::fs::set_permissions(&shared, std::fs::Permissions::from_mode(0o700)).unwrap();
        let target = Self::target_at(shared.to_str().unwrap());
        Self {
            root,
            target,
            handed_over: false,
        }
    }

    fn target_at(remote_tmp: &str) -> SshTarget {
        let host = Host {
            name: "web1".into(),
            vars: [("ansible_remote_tmp".to_string(), json!(remote_tmp))]
                .into_iter()
                .collect(),
        };
        let Transport::Ssh(target) = Transport::for_host(&host, &defaults()).unwrap() else {
            panic!("an ssh transport")
        };
        target
    }

    fn shared(&self) -> PathBuf {
        self.root.path().join("shared")
    }

    fn dir(&self) -> PathBuf {
        self.shared()
            .join(format!("volant-agent-{VERSION}-{}", effective_uid()))
    }

    fn agent(&self) -> PathBuf {
        self.dir().join("volant-agent")
    }

    fn marker(&self) -> PathBuf {
        self.root.path().join("marker")
    }

    /// What an attacker would plant: it prints the version line the probe looks for, and leaves
    /// a mark of having run.
    fn fake_agent(&self) -> String {
        format!(
            "#!/bin/sh\necho 'volant-agent {VERSION} planted'\necho ran >> '{}'\n",
            self.marker().display()
        )
    }

    fn plant(&self, dir_mode: u32, agent_mode: u32) {
        std::fs::create_dir(self.dir()).unwrap();
        std::fs::write(self.agent(), self.fake_agent()).unwrap();
        std::fs::set_permissions(self.agent(), std::fs::Permissions::from_mode(agent_mode))
            .unwrap();
        std::fs::set_permissions(self.dir(), std::fs::Permissions::from_mode(dir_mode)).unwrap();
    }

    /// Gives `path` to another user. `false` when this process can neither chown nor use
    /// `sudo` without a password, which skips the test with a message.
    fn hand_over(&mut self, path: &Path) -> bool {
        let mut chown = if effective_uid() == 0 {
            Command::new("chown")
        } else {
            let mut sudo = Command::new("sudo");
            sudo.args(["-n", "chown"]);
            sudo
        };
        let done = chown
            .args(["-R", &OTHER_UID.to_string()])
            .arg(path)
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|status| status.success());
        self.handed_over |= done;
        if !done {
            eprintln!("skipped: giving a directory to another user needs root or `sudo -n`");
        }
        done
    }

    fn sh(script: &str, stdin: &[u8], home: Option<&Path>) -> Output {
        let mut command = Command::new("sh");
        command
            .arg("-c")
            .arg(script)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if let Some(home) = home {
            command.env("HOME", home);
        }
        let mut child = command.spawn().unwrap();
        // A command that refuses early closes its input; the exit code says why.
        let _ = child.stdin.take().unwrap().write_all(stdin);
        child.wait_with_output().unwrap()
    }

    fn upload(&self, payload: &[u8]) -> Output {
        Self::sh(
            &self.target.upload_command(payload.len() as u64),
            payload,
            None,
        )
    }

    fn probe(&self) -> Output {
        Self::sh(&self.target.probe_command(), b"", None)
    }

    /// `script` run with an `ls` that says `remote_tmp` has `mode`, as a file system with ACLs
    /// does: a named-user ACL shows in the mode column as a group mask and a trailing `+`. With
    /// `foreign_group`, the same `ls` also gives `remote_tmp` a group other than this user's own.
    fn sh_with_remote_tmp_mode(
        &self,
        script: &str,
        stdin: &[u8],
        mode: &str,
        foreign_group: bool,
    ) -> Output {
        let bin = self.root.path().join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let fake = bin.join("ls");
        let group = if foreign_group {
            // SAFETY: `getgid` reads the calling process's own credentials and cannot fail.
            let other = unsafe { libc::getgid() } + 1;
            format!(" s/^\\([^ ]* *[^ ]* *[^ ]* *\\)[^ ]*/\\1{other}/;")
        } else {
            String::new()
        };
        std::fs::write(
            &fake,
            format!(
                "#!/bin/sh\n/bin/ls \"$@\" | sed '/\\/shared\\/$/ {{ s/^[^ ]*/{mode}/;{group} }}'\n"
            ),
        )
        .unwrap();
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
        let path = format!("{}:{}", bin.display(), std::env::var("PATH").unwrap());
        let mut child = Command::new("sh")
            .arg("-c")
            .arg(script)
            .env("PATH", path)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let _ = child.stdin.take().unwrap().write_all(stdin);
        child.wait_with_output().unwrap()
    }
}

impl Drop for Remote {
    fn drop(&mut self) {
        // What another user owns is beyond the scratch directory's own clean-up.
        if self.handed_over {
            let _ = Command::new("sudo")
                .args(["-n", "rm", "-rf"])
                .arg(self.root.path())
                .status();
        }
    }
}

fn effective_uid() -> u32 {
    // SAFETY: `geteuid` reads the calling process's own credentials and cannot fail.
    unsafe { libc::geteuid() }
}

fn captured(out: &Output) -> Captured {
    Captured {
        code: out.status.code(),
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
    }
}

/// What the cached agent would print for the probe: the version, then a hash of itself.
fn good_agent(hash: &str) -> Vec<u8> {
    format!("#!/bin/sh\necho 'volant-agent {VERSION} {hash}'\n").into_bytes()
}

fn mode(path: &Path) -> u32 {
    std::fs::metadata(path).unwrap().permissions().mode() & 0o7777
}

#[derive(Clone, Copy, Debug)]
enum Plant {
    /// Another user owns the directory and the program in it.
    ForeignDirAndAgent,
    /// Another user made the directory first and put nothing in it.
    ForeignEmptyDir,
    /// This user owns the directory, and anybody can write to it.
    OpenDir,
    /// This user owns a closed directory, and anybody can write to the program in it.
    OpenAgent,
    /// The directory is a link to another directory that holds a program.
    SymlinkedDir,
    /// The program is a link to a file somewhere else.
    SymlinkedAgent,
    /// A good earlier upload, then anybody can rename entries in `remote_tmp`.
    OpenParent,
}

/// The planted program never runs, and an upload either replaces it or refuses.
#[rstest]
#[case::another_users_directory(Plant::ForeignDirAndAgent, EXIT_UNSAFE, EXIT_UNSAFE, false)]
#[case::another_users_empty_directory(Plant::ForeignEmptyDir, EXIT_MISSING, EXIT_UNSAFE, false)]
#[case::a_directory_anybody_can_write(Plant::OpenDir, EXIT_UNSAFE, 0, true)]
#[case::a_program_anybody_can_write(Plant::OpenAgent, EXIT_UNSAFE, 0, true)]
#[case::a_linked_directory(Plant::SymlinkedDir, EXIT_UNSAFE, EXIT_UNSAFE, false)]
#[case::a_linked_program(Plant::SymlinkedAgent, EXIT_UNSAFE, 0, true)]
#[case::a_shared_remote_tmp_without_the_sticky_bit(
    Plant::OpenParent,
    EXIT_UNSAFE,
    EXIT_UNSAFE,
    false
)]
fn a_planted_agent_directory_is_never_run(
    #[case] plant: Plant,
    #[case] probe_code: i32,
    #[case] upload_code: i32,
    #[case] replaced: bool,
) {
    let mut remote = Remote::new();
    let elsewhere = remote.root.path().join("elsewhere");
    match plant {
        Plant::ForeignDirAndAgent => {
            remote.plant(0o755, 0o755);
            if !remote.hand_over(&remote.dir()) {
                return;
            }
        }
        Plant::ForeignEmptyDir => {
            std::fs::create_dir(remote.dir()).unwrap();
            if !remote.hand_over(&remote.dir()) {
                return;
            }
        }
        Plant::OpenDir => {
            remote.plant(0o777, 0o755);
            // What a link through `su` writes its stderr to, aimed at a file of the user's.
            symlink(
                remote.root.path().join("victim"),
                remote.dir().join("agent.stderr"),
            )
            .unwrap();
        }
        Plant::OpenAgent => remote.plant(0o700, 0o777),
        Plant::SymlinkedDir => {
            std::fs::create_dir(&elsewhere).unwrap();
            std::fs::write(elsewhere.join("volant-agent"), remote.fake_agent()).unwrap();
            std::fs::set_permissions(
                elsewhere.join("volant-agent"),
                std::fs::Permissions::from_mode(0o755),
            )
            .unwrap();
            symlink(&elsewhere, remote.dir()).unwrap();
        }
        Plant::SymlinkedAgent => {
            std::fs::create_dir(remote.dir()).unwrap();
            std::fs::set_permissions(remote.dir(), std::fs::Permissions::from_mode(0o700)).unwrap();
            std::fs::write(&elsewhere, remote.fake_agent()).unwrap();
            std::fs::set_permissions(&elsewhere, std::fs::Permissions::from_mode(0o755)).unwrap();
            symlink(&elsewhere, remote.agent()).unwrap();
        }
        Plant::OpenParent => {
            assert_eq!(remote.upload(&good_agent("one")).status.code(), Some(0));
            std::fs::set_permissions(remote.shared(), std::fs::Permissions::from_mode(0o777))
                .unwrap();
        }
    }

    let probe = remote.probe();
    assert_eq!(
        probe.status.code(),
        Some(probe_code),
        "{:?}",
        captured(&probe)
    );
    if probe_code == EXIT_UNSAFE {
        let said = String::from_utf8_lossy(&probe.stderr).into_owned();
        assert!(
            said.contains(remote.shared().to_str().unwrap()),
            "the refusal names the path: {said}"
        );
    }
    let upload = remote.upload(&good_agent("two"));
    assert_eq!(
        upload.status.code(),
        Some(upload_code),
        "{:?}",
        captured(&upload)
    );
    if upload_code == EXIT_UNSAFE {
        let said = String::from_utf8_lossy(&upload.stderr).into_owned();
        assert!(
            said.contains(remote.shared().to_str().unwrap()),
            "the refusal names the path: {said}"
        );
    }
    assert!(
        !remote.marker().exists(),
        "the planted program ran on the host"
    );
    let landed = std::fs::read(remote.agent()).unwrap_or_default();
    if replaced {
        assert_eq!(landed, good_agent("two"), "the upload replaced it");
        let probe = remote.probe();
        assert_eq!(probe.status.code(), Some(0), "{:?}", captured(&probe));
        assert!(is_this_agent(&captured(&probe), "two"));
        assert_eq!(
            mode(&remote.dir()) & 0o077,
            0,
            "the directory is private again"
        );
        assert!(
            std::fs::symlink_metadata(remote.dir().join("agent.stderr")).is_err(),
            "what was planted in the open directory is gone"
        );
        assert!(!remote.marker().exists());
    } else if let Plant::OpenParent = plant {
        assert_eq!(
            landed,
            good_agent("one"),
            "nothing was written under an open parent"
        );
    } else {
        assert_ne!(landed, good_agent("two"), "nothing was written into it");
    }
}

/// A normal run: the first upload makes a private directory, later runs reuse the program, a
/// directory from an earlier run is accepted, and clean-up touches only what this user owns.
#[test]
fn an_ordinary_run_still_caches_and_reuses_the_agent() {
    let mut remote = Remote::new();
    let payload = good_agent("abc");
    let upload = remote.upload(&payload);
    assert_eq!(upload.status.code(), Some(0), "{:?}", captured(&upload));
    assert_eq!(mode(&remote.dir()) & 0o077, 0, "a new directory is private");
    assert_eq!(
        mode(&remote.agent()) & 0o022,
        0,
        "nobody else can write the program"
    );
    let probe = remote.probe();
    assert!(
        is_this_agent(&captured(&probe), "abc"),
        "{:?}",
        captured(&probe)
    );
    let inode = std::fs::metadata(remote.agent()).unwrap();
    let again = remote.probe();
    assert!(is_this_agent(&captured(&again), "abc"));
    assert_eq!(
        std::fs::metadata(remote.agent())
            .unwrap()
            .modified()
            .unwrap(),
        inode.modified().unwrap(),
        "a probe changes nothing"
    );

    // `remote_tmp` may be this user's own and open to its own group (a umask of 002 makes it so), or
    // sticky and open to everybody, as `/tmp` is.
    for parent in [0o775, 0o1777, 0o755] {
        std::fs::set_permissions(remote.shared(), std::fs::Permissions::from_mode(parent)).unwrap();
        let probe = remote.probe();
        assert!(
            is_this_agent(&captured(&probe), "abc"),
            "{parent:o}: {:?}",
            captured(&probe)
        );
    }
    std::fs::set_permissions(remote.shared(), std::fs::Permissions::from_mode(0o700)).unwrap();

    // A directory an earlier run made with the usual umask is still good.
    std::fs::set_permissions(remote.dir(), std::fs::Permissions::from_mode(0o755)).unwrap();
    let probe = remote.probe();
    assert!(
        is_this_agent(&captured(&probe), "abc"),
        "{:?}",
        captured(&probe)
    );

    // Another version's directory of this user goes. Another user's stays.
    let old = remote.shared().join("volant-agent-0.0.1");
    std::fs::create_dir(&old).unwrap();
    // Set, not left to the umask: a group-writable directory is kept, and 002 makes it one.
    std::fs::set_permissions(&old, std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::write(old.join("volant-agent"), "old").unwrap();
    let theirs = remote.shared().join("volant-agent-0.0.2");
    std::fs::create_dir(&theirs).unwrap();
    std::fs::write(theirs.join("volant-agent"), "theirs").unwrap();
    let handed = remote.hand_over(&theirs);
    let upload = remote.upload(&payload);
    assert_eq!(upload.status.code(), Some(0), "{:?}", captured(&upload));
    assert!(!old.exists(), "an older directory of this user is removed");
    if handed {
        assert!(theirs.exists(), "another user's directory is left alone");
    }
    assert!(is_this_agent(&captured(&remote.probe()), "abc"));
}

/// The default `remote_tmp` is under the home directory, which nobody else can write.
#[test]
fn the_default_remote_tmp_under_home_works() {
    let home = scratch();
    let target = Remote::target_at("~/.ansible/tmp");
    let payload = good_agent("abc");
    let upload = Remote::sh(
        &target.upload_command(payload.len() as u64),
        &payload,
        Some(home.path()),
    );
    assert_eq!(upload.status.code(), Some(0), "{:?}", captured(&upload));
    let probe = Remote::sh(&target.probe_command(), b"", Some(home.path()));
    assert!(
        is_this_agent(&captured(&probe), "abc"),
        "{:?}",
        captured(&probe)
    );
}

/// The start command checks again, so a directory swapped after the probe is not run, and the
/// stderr file of a link through `su` lands in the directory it was checked in.
#[test]
fn the_start_command_checks_the_directory_again() {
    let remote = Remote::new();
    let script = format!(
        "#!/bin/sh\necho started >> '{}'\necho oops >&2\n",
        remote.marker().display()
    );
    assert_eq!(remote.upload(script.as_bytes()).status.code(), Some(0));

    let started = Remote::sh(
        &remote.target.exec_command(" 2>\"$d/agent.stderr\""),
        b"",
        None,
    );
    assert_eq!(started.status.code(), Some(0), "{:?}", captured(&started));
    assert_eq!(
        std::fs::read_to_string(remote.marker()).unwrap(),
        "started\n"
    );
    assert_eq!(
        std::fs::read_to_string(remote.dir().join("agent.stderr")).unwrap(),
        "oops\n"
    );

    // Somebody opens the directory after the probe, and swaps the program.
    std::fs::set_permissions(remote.dir(), std::fs::Permissions::from_mode(0o777)).unwrap();
    std::fs::remove_file(remote.marker()).unwrap();
    let refused = Remote::sh(&remote.target.exec_command(""), b"", None);
    assert_eq!(
        refused.status.code(),
        Some(EXIT_UNSAFE),
        "{:?}",
        captured(&refused)
    );
    assert!(
        String::from_utf8_lossy(&refused.stderr).contains(remote.dir().to_str().unwrap()),
        "{:?}",
        captured(&refused)
    );
    assert!(!remote.marker().exists(), "the program did not start");
}

/// A default ACL on `remote_tmp` replaces the umask, so a directory made with `mkdir` alone comes
/// out open to the group however tight the umask is. The upload asks for mode 0700 itself.
#[test]
fn a_default_acl_on_remote_tmp_does_not_open_the_directory() {
    let remote = Remote::new();
    let set = Command::new("setfacl")
        .args(["-d", "-m", "g::rwx,o::rx"])
        .arg(remote.shared())
        .stderr(Stdio::null())
        .status();
    if !set.is_ok_and(|status| status.success()) {
        eprintln!("skipped: `setfacl` is missing, or the file system has no ACLs");
        return;
    }
    let upload = remote.upload(&good_agent("abc"));
    assert_eq!(upload.status.code(), Some(0), "{:?}", captured(&upload));
    assert_eq!(mode(&remote.dir()) & 0o077, 0, "the directory is private");
    let probe = remote.probe();
    assert!(
        is_this_agent(&captured(&probe), "abc"),
        "{:?}",
        captured(&probe)
    );
}

/// Group write on `remote_tmp` is refused under a named ACL (`+`) or a group not the user's own;
/// the sticky bit makes it safe again.
#[rstest]
#[case::a_named_acl_with_write("drwxrwx---+", false, false)]
#[case::the_same_without_an_acl("drwxrwx---", false, true)]
#[case::group_write_for_another_group("drwxrwx---", true, false)]
#[case::a_named_acl_and_the_sticky_bit("drwxrwx--t+", false, true)]
fn a_remote_tmp_open_to_a_named_acl_is_refused(
    #[case] mode: &str,
    #[case] foreign_group: bool,
    #[case] accepted: bool,
) {
    let remote = Remote::new();
    let upload = remote.upload(&good_agent("abc"));
    assert_eq!(upload.status.code(), Some(0), "{:?}", captured(&upload));
    let probe =
        remote.sh_with_remote_tmp_mode(&remote.target.probe_command(), b"", mode, foreign_group);
    let upload = remote.sh_with_remote_tmp_mode(
        &remote.target.upload_command(3),
        b"abc",
        mode,
        foreign_group,
    );
    if accepted {
        assert!(
            is_this_agent(&captured(&probe), "abc"),
            "{:?}",
            captured(&probe)
        );
        assert_ne!(
            upload.status.code(),
            Some(EXIT_UNSAFE),
            "{:?}",
            captured(&upload)
        );
    } else {
        assert_eq!(
            probe.status.code(),
            Some(EXIT_UNSAFE),
            "{:?}",
            captured(&probe)
        );
        assert_eq!(
            upload.status.code(),
            Some(EXIT_UNSAFE),
            "{:?}",
            captured(&upload)
        );
        let said = String::from_utf8_lossy(&probe.stderr).into_owned();
        assert!(said.contains(remote.shared().to_str().unwrap()), "{said}");
    }
}

/// The clean-up keeps an older version's directory that another user could write to.
#[test]
fn an_open_older_directory_is_kept() {
    let remote = Remote::new();
    let open = remote.shared().join("volant-agent-0.0.1");
    std::fs::create_dir(&open).unwrap();
    std::fs::set_permissions(&open, std::fs::Permissions::from_mode(0o777)).unwrap();
    let closed = remote.shared().join("volant-agent-0.0.2");
    std::fs::create_dir(&closed).unwrap();
    std::fs::set_permissions(&closed, std::fs::Permissions::from_mode(0o755)).unwrap();
    let upload = remote.upload(&good_agent("abc"));
    assert_eq!(upload.status.code(), Some(0), "{:?}", captured(&upload));
    assert!(open.exists(), "a directory others can write is not removed");
    assert!(!closed.exists(), "a closed one of this user is");
}

/// The same with a real ACL entry for another user, where `setfacl` can set one.
#[test]
fn a_real_acl_entry_for_another_user_on_remote_tmp_is_refused() {
    let remote = Remote::new();
    assert_eq!(remote.upload(&good_agent("abc")).status.code(), Some(0));
    let set = Command::new("setfacl")
        .args(["-m", &format!("u:{OTHER_UID}:rwx")])
        .arg(remote.shared())
        .stderr(Stdio::null())
        .status();
    if !set.is_ok_and(|status| status.success()) {
        eprintln!("skipped: `setfacl` is missing, or the file system has no ACLs");
        return;
    }
    let probe = remote.probe();
    assert_eq!(
        probe.status.code(),
        Some(EXIT_UNSAFE),
        "{:?}",
        captured(&probe)
    );
    let upload = remote.upload(&good_agent("two"));
    assert_eq!(
        upload.status.code(),
        Some(EXIT_UNSAFE),
        "{:?}",
        captured(&upload)
    );
}

/// The probe and the start in one command start the cached agent only when it is the expected
/// build, in a directory the checks pass, and otherwise exit 48 with nothing on stdout.
#[test]
fn the_fused_command_starts_only_the_expected_agent() {
    let line = |hash: &str| format!("volant-agent {VERSION} {hash}");
    let remote = Remote::new();
    let fused = |hash: &str| remote.target.fused_command(&[("*", line(hash))]);
    let missing = Remote::sh(&fused("abc"), b"", None);
    assert_eq!((missing.status.code(), missing.stdout.len()), (Some(48), 0));

    let agent = format!(
        "#!/bin/sh\n[ \"$1\" = --version ] && {{ echo '{}'; exit 0; }}\necho started\n",
        line("abc")
    );
    let upload = remote.upload(agent.as_bytes());
    assert_eq!(upload.status.code(), Some(0), "{:?}", captured(&upload));
    let started = Remote::sh(&fused("abc"), b"", None);
    assert_eq!(
        (
            started.status.code(),
            String::from_utf8_lossy(&started.stdout).trim()
        ),
        (Some(0), "started"),
        "{:?}",
        captured(&started)
    );
    let other = Remote::sh(&fused("def"), b"", None);
    assert_eq!((other.status.code(), other.stdout.len()), (Some(48), 0));

    // A directory open to others is never run from, whatever its program says.
    std::fs::set_permissions(remote.dir(), std::fs::Permissions::from_mode(0o777)).unwrap();
    let open = Remote::sh(&fused("abc"), b"", None);
    assert_eq!((open.status.code(), open.stdout.len()), (Some(48), 0));
}
