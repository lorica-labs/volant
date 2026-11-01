// SPDX-License-Identifier: GPL-3.0-or-later
//! What a local `sudo` said before the agent started, put in front of each `raw` task's output
//! as the reference's local connection keeps it with `become_strip_preamble: false`. The
//! reference runs `sudo` for every task; a link runs it once, so the words it kept stand for
//! those of each task.

use serde_json::Value;
use tokio::io::AsyncRead;
use tokio::process::Child;
use volant_protocol::modules::short_name;
use volant_protocol::{FromAgent, ToAgent};

use super::{AgentLink, Input};

/// The stdout and stderr kept, and which tasks of the batch last sent are `raw`.
pub(super) struct Preamble {
    stdout: String,
    stderr: String,
    raw: (u64, Vec<bool>),
}

impl AgentLink {
    /// [`AgentLink::new`] over pipes of the child already taken, a part of its stdout read, with
    /// the `preamble` its `raw` tasks show.
    pub fn new_over(
        child: Child,
        stdin: Input,
        stdout: Box<dyn AsyncRead + Send + Unpin>,
        preamble: Option<(Vec<u8>, Vec<u8>)>,
    ) -> Self {
        let mut link = Self::over(child, stdin, stdout).0;
        link.preamble = preamble.map(|(stdout, stderr)| Preamble {
            stdout: String::from_utf8_lossy(&stdout).into_owned(),
            stderr: String::from_utf8_lossy(&stderr).into_owned(),
            raw: (0, Vec::new()),
        });
        link
    }

    /// Notes which tasks of a batch about to be sent are `raw`.
    pub(super) fn note_raw(&mut self, msg: &ToAgent) {
        if let (Some(preamble), ToAgent::RunBatch { id, tasks }) = (&mut self.preamble, msg) {
            let raw = tasks.iter().map(|t| short_name(&t.module) == "raw");
            preamble.raw = (*id, raw.collect());
        }
    }

    /// Puts the preamble in front of a `raw` task's stdout and stderr, and their lines.
    pub(super) fn with_preamble(&self, answer: &mut FromAgent) {
        if let Some(preamble) = &self.preamble {
            prepend(preamble, answer);
        }
    }
}

fn prepend(preamble: &Preamble, answer: &mut FromAgent) {
    let FromAgent::TaskResult {
        batch,
        index,
        result,
        ..
    } = answer
    else {
        return;
    };
    if preamble.raw.0 != *batch || preamble.raw.1.get(*index) != Some(&true) {
        return;
    }
    for (key, before) in [("stdout", &preamble.stdout), ("stderr", &preamble.stderr)] {
        let Some(Value::String(text)) = result.0.get_mut(key) else {
            continue;
        };
        text.insert_str(0, before);
        let lines: Vec<Value> = text.lines().map(|l| Value::String(l.into())).collect();
        let key = format!("{key}_lines");
        if result.0.contains_key(&key) {
            result.0.insert(key, Value::Array(lines));
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use volant_protocol::{FromAgent, TaskResult};

    use super::{Preamble, prepend};

    #[test]
    fn a_raw_task_shows_what_sudo_said_first() {
        let preamble = Preamble {
            stdout: "stdout noise\n".into(),
            stderr: "stderr noise\n".into(),
            raw: (7, vec![false, true]),
        };
        let result = |stdout: &str| {
            let map = json!({"stdout": stdout, "stdout_lines": [stdout], "stderr": ""});
            TaskResult(map.as_object().unwrap().clone())
        };
        let mut answers = [0, 1].map(|index| FromAgent::TaskResult {
            batch: 7,
            index,
            result: result("deploy ran"),
            ran: None,
        });
        for answer in &mut answers {
            prepend(&preamble, answer);
        }
        let FromAgent::TaskResult { result, .. } = &answers[1] else {
            unreachable!()
        };
        assert_eq!(result.0["stdout"], "stdout noise\ndeploy ran");
        assert_eq!(
            result.0["stdout_lines"],
            json!(["stdout noise", "deploy ran"])
        );
        assert_eq!(result.0["stderr"], "stderr noise\n");
        let FromAgent::TaskResult { result, .. } = &answers[0] else {
            unreachable!()
        };
        assert_eq!(result.0["stdout"], "deploy ran");
    }
}
