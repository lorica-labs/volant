// SPDX-License-Identifier: GPL-3.0-or-later
//! The `ssh` command lines of an `SshTarget`.

use super::bootstrap::{
    EXIT_AGENT_MISSING, EXIT_AGENT_UNRUNNABLE, EXIT_NO_SPACE, EXIT_SHORT_WRITE, EXIT_UNAME_FAILED,
    space_needed_kib,
};
use super::control::{CONTROL_PERSIST, SERVER_ALIVE_COUNT_MAX, SERVER_ALIVE_INTERVAL, SSH_PROGRAM};
use super::defaults::SshTarget;
use super::shell::shell_word;

impl SshTarget {
    /// The `ssh` command line for one remote command. Options first, then the host, then `--`.
    pub fn ssh_argv(&self, remote_command: &str) -> Vec<String> {
        self.ssh_argv_with(remote_command, false)
    }

    /// The same, with `-C` when the command carries a payload on stdin. Compression is an
    /// option of `ssh`, so it belongs before the address: everything from the address onwards
    /// is the remote command's own argument list, and an option appended there would reach
    /// the remote shell instead.
    pub fn ssh_argv_with(&self, remote_command: &str, compress: bool) -> Vec<String> {
        let mut argv = vec![SSH_PROGRAM.to_string()];
        if compress {
            argv.push("-C".into());
        }
        argv.extend([
            "-o".to_string(),
            // A password given with `-k` or a password file comes through `SSH_ASKPASS`, which
            // `BatchMode` would switch off.
            if crate::askpass::installed() {
                "BatchMode=no"
            } else {
                "BatchMode=yes"
            }
            .into(),
            "-o".into(),
            format!("ConnectTimeout={}", self.connect_timeout.as_secs()),
        ]);
        if crate::askpass::installed() {
            // One wrong answer is the end of it, as in the reference, not three tries.
            argv.extend(["-o".into(), "NumberOfPasswordPrompts=1".into()]);
        }
        if let Some(path) = &self.control_path {
            argv.extend([
                "-o".into(),
                "ControlMaster=auto".into(),
                "-o".into(),
                format!("ControlPath={}", path.display()),
                "-o".into(),
                format!("ControlPersist={CONTROL_PERSIST}"),
            ]);
            // A master left on a connection its host dropped without a word exits on its own
            // after three unanswered probes, about 15 seconds, instead of holding every new
            // session until TCP gives up. The operator's own keepalive, if they set one, wins.
            if !self
                .common_args
                .iter()
                .chain(&self.extra_args)
                .any(|word| word.to_ascii_lowercase().contains("serveralive"))
            {
                argv.extend([
                    "-o".into(),
                    format!("ServerAliveInterval={SERVER_ALIVE_INTERVAL}"),
                    "-o".into(),
                    format!("ServerAliveCountMax={SERVER_ALIVE_COUNT_MAX}"),
                ]);
            }
        }
        if !self.host_key_checking {
            argv.extend([
                "-o".into(),
                "StrictHostKeyChecking=no".into(),
                "-o".into(),
                "UserKnownHostsFile=/dev/null".into(),
            ]);
        }
        if let Some(port) = self.port {
            argv.extend(["-p".into(), port.to_string()]);
        }
        if let Some(key) = &self.private_key {
            argv.extend(["-i".into(), key.display().to_string()]);
        }
        argv.extend(self.common_args.iter().cloned());
        argv.extend(self.extra_args.iter().cloned());
        if let Some(user) = &self.user {
            argv.extend(["-l".into(), user.clone()]);
        }
        argv.extend([
            self.address.clone(),
            "--".into(),
            remote_command.to_string(),
        ]);
        argv
    }

    pub(super) fn cache_dir(&self) -> String {
        format!(
            "{}/volant-agent-{}",
            self.remote_tmp,
            env!("CARGO_PKG_VERSION")
        )
    }

    /// Where the agent itself sits on the host, as this controller writes it in a message.
    pub(super) fn agent_path(&self) -> String {
        format!("{}/volant-agent", self.cache_dir())
    }

    /// Prints the machine architecture, then the cached agent's version and the hash of its own
    /// file, or exits 42 when no agent is cached. The architecture names the binary this
    /// controller would upload, whose hash decides whether the cached one is reused. A cached
    /// agent that cannot run exits 45 and `uname` failing exits 46, so neither hands its own
    /// status back to `ssh`, where 255 would read as a connection failure.
    pub fn probe_command(&self) -> String {
        format!(
            "uname -m || exit {EXIT_UNAME_FAILED}; a={agent}; if [ -x \"$a\" ]; then \"$a\" --version --build-id || {{ echo \"exit status $?\" >&2; exit {EXIT_AGENT_UNRUNNABLE}; }}; else exit {EXIT_AGENT_MISSING}; fi",
            agent = shell_word(&self.agent_path())
        )
    }

    /// Reads the agent from stdin into the cache, atomically, after checking free space. It
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
    pub fn upload_command(&self, size: u64) -> String {
        format!(
            "d={dir}; mkdir -p \"$d\" || exit 1; \
             t=\"$d/volant-agent.tmp.$$\"; \
             avail=$(df -Pk \"$d\" | awk 'NR==2 {{print $4}}'); \
             case \"$avail\" in ''|*[!0-9]*) avail=0;; esac; \
             [ \"$avail\" -ge {need} ] || {{ echo \"$avail\"; exit {EXIT_NO_SPACE}; }}; \
             head -c {size} > \"$t\" || {{ rm -f \"$t\"; exit 1; }}; \
             [ \"$(wc -c < \"$t\")\" -eq {size} ] || {{ rm -f \"$t\"; exit {EXIT_SHORT_WRITE}; }}; \
             chmod 755 \"$t\" || {{ rm -f \"$t\"; exit 1; }}; \
             mv \"$t\" \"$d/volant-agent\" || {{ rm -f \"$t\"; exit 1; }}; \
             find \"$d\" -name 'volant-agent.tmp.*' -mmin +60 -delete 2>/dev/null; \
             for old in {tmp}/volant-agent-*; do [ \"$old\" = \"$d\" ] || rm -rf \"$old\"; done; \
             exit 0",
            dir = shell_word(&self.cache_dir()),
            need = space_needed_kib(size),
            tmp = shell_word(&self.remote_tmp),
        )
    }
}
