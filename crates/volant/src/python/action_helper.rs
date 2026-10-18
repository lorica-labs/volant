// SPDX-License-Identifier: GPL-3.0-or-later

use volant_protocol::frame::{read_frame, write_frame};

use super::builder::PythonBuilder;

/// What the thread reading an [`ActionHelper`]'s output hands over: a frame, `None` once the
/// helper closed its output, or why the read failed.
pub(crate) type Frame = std::io::Result<Option<Vec<u8>>>;

/// A helper that runs action plugins. Its frames are read on a thread of their own, so the wait
/// for the next one ends at the task's deadline even when the plugin never answers.
pub(crate) struct ActionHelper {
    helper: PythonBuilder,
    frames: std::sync::mpsc::Receiver<Frame>,
}

/// Why no frame came from an [`ActionHelper`].
pub(crate) enum Silence {
    /// The deadline passed first.
    Expired,
    /// The reading thread is gone, which happens only once the helper's output has ended.
    Gone,
}

impl ActionHelper {
    pub(crate) fn new(mut helper: PythonBuilder) -> ActionHelper {
        let mut stdout = helper
            .stdout
            .take()
            .expect("a new helper's output is unread");
        let (send, frames) = std::sync::mpsc::channel();
        // The thread ends with the helper's output: at its exit, or once it is killed or dropped.
        std::thread::spawn(move || {
            loop {
                let frame = read_frame(&mut stdout);
                let last = !matches!(frame, Ok(Some(_)));
                if send.send(frame).is_err() || last {
                    return;
                }
            }
        });
        ActionHelper { helper, frames }
    }

    pub(crate) fn send(&mut self, frame: &[u8]) -> std::io::Result<()> {
        let stdin = self
            .helper
            .stdin
            .as_mut()
            .expect("stdin is open until drop");
        write_frame(stdin, frame)
    }

    /// The next frame, waiting no later than `deadline`.
    pub(crate) fn next(&self, deadline: std::time::Instant) -> Result<Frame, Silence> {
        use std::sync::mpsc::RecvTimeoutError;

        let left = deadline.saturating_duration_since(std::time::Instant::now());
        self.frames.recv_timeout(left).map_err(|err| match err {
            RecvTimeoutError::Timeout => Silence::Expired,
            RecvTimeoutError::Disconnected => Silence::Gone,
        })
    }

    /// Ends the helper and says how it went.
    pub(crate) fn end(&mut self) -> String {
        self.helper.end()
    }

    /// Whether the process still runs: one that died while idle is not handed a request.
    pub(crate) fn alive(&mut self) -> bool {
        matches!(self.helper.child.try_wait(), Ok(None))
    }

    /// Asks the helper to stop, without waiting: closing its input ends its loop.
    pub(crate) fn close_input(&mut self) {
        drop(self.helper.stdin.take());
    }
}
