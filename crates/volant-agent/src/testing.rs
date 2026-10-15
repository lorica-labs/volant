// SPDX-License-Identifier: GPL-3.0-or-later
//! Shapes more than one module's tests build.

use serde_json::{Map, Value};

/// A directory removed when it goes out of scope, however the test ends.
pub(crate) fn scratch() -> tempfile::TempDir {
    tempfile::tempdir().expect("a scratch directory")
}

/// The arguments of a task, written as a JSON object.
pub(crate) fn args(json: Value) -> Map<String, Value> {
    json.as_object().expect("an object").clone()
}

/// The result of a run that was not cancelled.
#[cfg(unix)]
pub(crate) fn done(run: crate::modules::Run) -> volant_protocol::TaskResult {
    use crate::modules::Run;
    match run {
        Run::Done(result) => result,
        Run::Cancelled => panic!("the task was cancelled"),
    }
}
