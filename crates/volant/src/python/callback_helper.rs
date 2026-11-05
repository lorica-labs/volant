// SPDX-License-Identifier: GPL-3.0-or-later
//! The helper that runs Python callback plugins, apart from the run's other helpers: what a plugin
//! prints goes to the run's own stdout, so its frames cross a socket given as its stdin.

use serde_json::Value;

/// The helper's source: `callbacks.py`.
#[cfg_attr(not(unix), allow(dead_code))]
const SOURCE: &str = include_str!("callbacks.py");

/// How long the helper gets to finish its exit handlers once the run is over.
#[cfg_attr(not(unix), allow(dead_code))]
const EXIT_GRACE: std::time::Duration = std::time::Duration::from_secs(2);

/// One running callback helper.
pub struct CallbackHelper {
    #[cfg(unix)]
    child: std::process::Child,
    #[cfg(unix)]
    channel: std::os::unix::net::UnixStream,
}

#[cfg(unix)]
impl CallbackHelper {
    /// Starts it under the interpreter the run's other helpers use.
    ///
    /// # Errors
    /// No interpreter has ansible-core, or it did not start.
    pub fn start() -> anyhow::Result<CallbackHelper> {
        use std::os::fd::OwnedFd;
        use std::process::{Command, Stdio};

        let explicit = std::env::var("VOLANT_PYTHON").ok();
        let virtual_env = std::env::var("VIRTUAL_ENV").ok();
        let python = super::find_python(explicit.as_deref(), virtual_env.as_deref(), true)?;
        let (channel, theirs) = std::os::unix::net::UnixStream::pair()?;
        let child = Command::new(python)
            .args([crate::runtime::resolve::NO_CWD_IMPORTS, "-c", SOURCE])
            .envs(super::builder::helper_env())
            .stdin(Stdio::from(OwnedFd::from(theirs)))
            .spawn()?;
        Ok(CallbackHelper { child, channel })
    }

    /// One request and its answer.
    ///
    /// # Errors
    /// What the helper answered instead, or why it did not answer.
    pub fn ask(&mut self, request: &Value) -> Result<Value, String> {
        use volant_protocol::frame::{read_frame, write_frame};

        write_frame(&mut self.channel, request.to_string().as_bytes())
            .map_err(|err| err.to_string())?;
        let frame = read_frame(&mut self.channel)
            .map_err(|err| err.to_string())?
            .ok_or("the Python callback helper exited")?;
        let answer: Value = serde_json::from_slice(&frame).map_err(|err| err.to_string())?;
        match answer.get("error").and_then(Value::as_str) {
            Some(why) => Err(why.to_string()),
            None => Ok(answer),
        }
    }
}

#[cfg(unix)]
impl Drop for CallbackHelper {
    /// The end of its input ends the helper's loop, which runs the exit handlers a plugin left.
    fn drop(&mut self) {
        let _ = self.channel.shutdown(std::net::Shutdown::Both);
        let deadline = std::time::Instant::now() + EXIT_GRACE;
        while std::time::Instant::now() < deadline {
            match self.child.try_wait() {
                Ok(Some(_)) | Err(_) => return,
                Ok(None) => std::thread::sleep(std::time::Duration::from_millis(10)),
            }
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[cfg(not(unix))]
impl CallbackHelper {
    /// ansible-core has no controller on this platform.
    ///
    /// # Errors
    /// Always.
    pub fn start() -> anyhow::Result<CallbackHelper> {
        anyhow::bail!("Python callback plugins need a Unix controller")
    }

    /// Nothing starts here, so nothing is asked.
    ///
    /// # Errors
    /// Always.
    pub fn ask(&mut self, _request: &Value) -> Result<Value, String> {
        Err("Python callback plugins need a Unix controller".into())
    }
}
