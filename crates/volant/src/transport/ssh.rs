// SPDX-License-Identifier: GPL-3.0-or-later
//! The `ssh` command lines of an `SshTarget`, and what the bootstrap needs of them.

use super::bootstrap::{Captured, EXIT_SSH_FAILURE};
use super::control::{CONTROL_PERSIST, SERVER_ALIVE_COUNT_MAX, SERVER_ALIVE_INTERVAL};
use super::defaults::{ConnectError, SshTarget};
use super::escalation::unreachable_message;
use super::reach::Reach;

impl SshTarget {
    /// Which password an `ssh` call of this target offers: the host variable's, or the one of
    /// the command line when the host names none.
    fn password_source(&self) -> crate::askpass::Source<'_> {
        match &self.own.password {
            Some(password) => crate::askpass::Source::Variable(password.expose()),
            None => crate::askpass::Source::Run,
        }
    }

    /// Whether an `ssh` call of this target offers a password.
    pub(super) fn offers_password(&self) -> bool {
        crate::askpass::offers(self.password_source())
    }

    /// The environment an `ssh` call of this target is started with, which hands it the password
    /// through `SSH_ASKPASS` when there is one.
    pub(super) fn askpass_environment(
        &self,
    ) -> Result<Vec<(&'static str, std::ffi::OsString)>, ConnectError> {
        crate::askpass::environment(self.password_source())
            .map_err(|e| ConnectError::Unreachable(format!("storing the connection password: {e}")))
    }

    /// The `ssh` command line for one remote command. Options first, then the host, then `--`.
    pub fn ssh_argv(&self, remote_command: &str) -> Vec<String> {
        self.ssh_argv_with(remote_command, false)
    }

    /// The same, with `-C` when the command carries a payload on stdin. Compression is an
    /// option of `ssh`, so it belongs before the address: everything from the address onwards
    /// is the remote command's own argument list, and an option appended there would reach
    /// the remote shell instead.
    pub fn ssh_argv_with(&self, remote_command: &str, compress: bool) -> Vec<String> {
        let offers_password = self.offers_password();
        let mut argv = vec![self.own.executable.clone()];
        if compress {
            argv.push("-C".into());
        }
        argv.extend([
            "-o".to_string(),
            // A password given with `-k` or a password file comes through `SSH_ASKPASS`, which
            // `BatchMode` would switch off.
            if offers_password {
                "BatchMode=no"
            } else {
                "BatchMode=yes"
            }
            .into(),
            "-o".into(),
            format!("ConnectTimeout={}", self.connect_timeout.as_secs()),
        ]);
        if offers_password {
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
        if let Some(key) = &self.own.private_key {
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
}

impl Reach for SshTarget {
    fn place(&self) -> &str {
        &self.address
    }

    fn remote_tmp(&self) -> &str {
        &self.remote_tmp
    }

    fn command_line(&self, command: &str, payload: bool) -> Vec<String> {
        self.ssh_argv_with(command, payload)
    }

    fn environment(&self) -> Result<Vec<(std::ffi::OsString, std::ffi::OsString)>, ConnectError> {
        Ok(self
            .askpass_environment()?
            .into_iter()
            .map(|(key, value)| (key.into(), value))
            .collect())
    }

    fn trace_exec(&self, argv: &[String]) {
        super::trace::exec(self, argv);
    }

    fn trace_result(&self, captured: &Captured) {
        super::trace::result(&self.address, captured);
    }

    /// `ssh` reports its own failures as 255, and none of the bootstrap commands ever returns it,
    /// so this cannot swallow a remote command's own status.
    fn connection_failure(
        &self,
        captured: &Captured,
        offered: Option<&str>,
    ) -> Option<ConnectError> {
        (captured.code == Some(EXIT_SSH_FAILURE))
            .then(|| ConnectError::Unreachable(unreachable_message(&captured.stderr, offered)))
    }

    fn over_ssh(&self) -> Option<&SshTarget> {
        Some(self)
    }
}
