// SPDX-License-Identifier: GPL-3.0-or-later
//! Where a task's controller file is found, and how its bytes travel to the host.

use std::path::{Component, Path, PathBuf};

use serde_json::Value;
use volant_protocol::TaskResult;
use volant_protocol::encoding::b64_encode;
use volant_protocol::frame::MAX_FRAME_LEN;

use std::collections::BTreeSet;

use super::FileBlob;
use crate::compile::Origin;
use crate::config::HostNamedPaths;

/// The largest file one blob carries: one frame, which holds the blob's bytes alone, raw or
/// deflated to fewer.
pub(crate) const MAX_FILE_LEN: usize = MAX_FRAME_LEN;

/// Where the reference looks for a task's local file, in its order.
///
/// Measured on ansible-core 2.19.12 from its "Searched in" list: `<role>/<subdir>/`, `<role>/`,
/// `<parent role>/<subdir>/`, `<parent role>/` for each role the role is a dependency of,
/// `<role>/tasks/<subdir>/`, `<role>/tasks/`, `<playbook>/<subdir>/`, `<playbook>/`. The task's
/// own directory stands where `<role>/tasks/` is, so a task outside a role searches its own
/// directory and the playbook's, which for a task written in the playbook is the same one twice,
/// as the reference lists it. A name that already starts with `<subdir>/` is not looked for
/// under `<subdir>/` again, and an absolute one is taken as it is.
pub(crate) fn search_paths(
    origin: &Origin,
    playbook_dir: &Path,
    subdir: &str,
    name: &str,
) -> Vec<PathBuf> {
    let named = Path::new(name);
    if named.is_absolute() {
        return vec![named.to_path_buf()];
    }
    let under = name.split('/').next() != Some(subdir);
    let mut out = Vec::new();
    for base in origin.search_dirs().chain([playbook_dir]) {
        if under {
            out.push(base.join(subdir).join(name));
        }
        out.push(base.join(name));
    }
    out
}

/// The reference's refusal when none of the [`search_paths`] is there, which ends with the hint
/// about `remote_src`. An absolute name is not searched for, so the list is left out.
pub(crate) fn not_found(name: &str, searched: &[PathBuf]) -> String {
    let hint = "If you are using a module and expect the file to exist on the remote, see the remote_src option";
    if Path::new(name).is_absolute() {
        return format!(
            "Could not find or access '{name}' on the Ansible Controller.
{hint}"
        );
    }
    let searched: Vec<String> = searched.iter().map(|p| p.display().to_string()).collect();
    format!(
        "Could not find or access '{name}'
Searched in:
	{} on the Ansible Controller.
{hint}",
        searched.join(
            "
	"
        )
    )
}

/// What `host_named_paths` says about a controller source `name` that `untrusted` lists `arg`
/// as named by a managed host, made before the file is read. Where the text of the name decides,
/// nothing is looked up: an answer of "not found" would tell a host which paths exist.
///
/// Measured on ansible-core 2.19.12, the reference sends whatever controller file a registered
/// value or a gathered fact names, `~/.ssh` included, so a host that controls a command's output
/// can have any file the operator can read sent to it. `Allow` does the same. `Refuse` refuses
/// every such name. `Contained` accepts a name that stays inside the `files` or `templates`
/// directory of the role, the roles it is a dependency of, the task's own directory or the
/// playbook, which are the directories [`search_paths`] looks in, and nothing else in those
/// trees:
///
/// - A name that starts with `~` is refused. The reference's own plugins expand `~` and `~user`
///   to a home directory before they look, so the text would not mean what it says.
/// - A name with a `..` component is refused, even one that folds back inside: the file system
///   follows a link before it folds, so the folded path is not the one that is opened.
/// - An absolute name must lie under one of those directories, and may not hold a `$`, which the
///   reference expands in an absolute name.
/// - A relative name is joined to the search directories. The first file that exists is the one
///   read, and it must lie under one of those directories: `secret.yml` beside the playbook is
///   outside `files`, so it is refused when it exists there.
///
/// The check reads the text of the path. A link that the author put inside those directories is
/// the author's own file and is followed.
pub(crate) fn check_host_named(
    mode: HostNamedPaths,
    untrusted: &BTreeSet<String>,
    arg: &str,
    name: &str,
    subdir: &str,
    origin: &Origin,
    playbook_dir: &Path,
) -> Result<(), String> {
    if !untrusted.contains(arg) {
        return Ok(());
    }
    match mode {
        HostNamedPaths::Allow => Ok(()),
        HostNamedPaths::Refuse => Err(format!(
            "the '{arg}' of this task was named by a managed host, and a controller file a host chose is never sent"
        )),
        HostNamedPaths::Contained => {
            if inside_task_directories(origin, playbook_dir, subdir, name) {
                Ok(())
            } else {
                Err(contained_refusal(arg, name))
            }
        }
    }
}

/// The refusal of the `contained` mode for the argument `arg` named `name`.
pub(crate) fn contained_refusal(arg: &str, name: &str) -> String {
    format!(
        "the '{arg}' of this task was named by a managed host, and '{name}' is not inside the files or templates directories of the playbook or the role (host_named_paths = contained)"
    )
}

/// A value as ansible-core's `boolean(value, strict=False)` reads it: `y`, `yes`, `on`, `1`,
/// `true` and `t` in any case and with spaces around, the numbers 1 and 1.0, and `true` are
/// true; everything else, `no`, `n`, `0`, `1.0` as text and `2` included, is false.
pub(crate) fn ansible_bool(value: &Value) -> bool {
    match value {
        Value::Bool(b) => *b,
        Value::String(text) => matches!(
            text.trim().to_lowercase().as_str(),
            "y" | "yes" | "on" | "1" | "true" | "t"
        ),
        Value::Number(number) => number.as_f64() == Some(1.0),
        _ => false,
    }
}

/// Whether the file `name` stands for lies under the `files` or `templates` directory of a
/// directory [`search_paths`] looks in.
fn inside_task_directories(origin: &Origin, playbook_dir: &Path, subdir: &str, name: &str) -> bool {
    let named = Path::new(name);
    if name.starts_with('~') || named.components().any(|c| c == Component::ParentDir) {
        return false;
    }
    let roots: Vec<PathBuf> = origin
        .search_dirs()
        .chain([playbook_dir])
        .flat_map(|base| [base.join("files"), base.join("templates")])
        .collect();
    let under = |path: &Path| roots.iter().any(|root| path.starts_with(root));
    if named.is_absolute() {
        return !name.contains('$') && under(named);
    }
    search_paths(origin, playbook_dir, subdir, name)
        .iter()
        .find(|path| path.exists())
        .is_none_or(|path| under(path))
}

#[cfg(test)]
thread_local! {
    /// The frame limit the tests of this thread run with, so a test of the refusal does not build
    /// a payload of the real size.
    pub(crate) static FRAME_LIMIT: std::cell::Cell<usize> = const { std::cell::Cell::new(MAX_FILE_LEN) };
}

/// The most one frame carries: [`MAX_FILE_LEN`], or the test thread's own limit.
fn frame_limit() -> usize {
    #[cfg(test)]
    return FRAME_LIMIT.get();
    #[cfg(not(test))]
    MAX_FILE_LEN
}

/// Refuses `len` bytes that could not fit in one frame, naming both sizes.
pub(crate) fn fits(name: &str, len: usize) -> Result<(), String> {
    let limit = frame_limit();
    if len > limit {
        return Err(format!(
            "{name} is {len} bytes; one frame carries at most {limit}"
        ));
    }
    Ok(())
}

/// A file's bytes as a blob, refused when they could not fit in one frame.
///
/// Named by the blake3 of the bytes, as the union is: the agent refuses a blob whose bytes hash
/// to anything else.
pub(crate) fn blob_of(name: &str, bytes: &[u8]) -> Result<FileBlob, String> {
    fits(name, bytes.len())?;
    Ok(FileBlob {
        hash: blake3::hash(bytes).to_hex().to_string(),
        b64: b64_encode(bytes),
    })
}

#[cfg(test)]
mod tests;

/// How the reference's loader words a source file it cannot read.
const UNREADABLE: &str = "an error occurred while trying to read the file ";

/// Why the controller file `found` could not be read: the loader's sentence with Python's
/// `OSError` behind it, measured on ansible-core 2.19.12.
pub(super) fn unreadable(found: &Path, err: &std::io::Error) -> String {
    match err.raw_os_error() {
        Some(errno) => {
            let shown = found.display();
            let text = err.to_string();
            let reason = text.split(" (os error").next().unwrap_or(&text);
            format!("{UNREADABLE}'{shown}': [Errno {errno}] {reason}: b'{shown}'")
        }
        None => format!("could not read src={}: {err}", found.display()),
    }
}

/// The failure of a source [`unreadable`] worded: raised by the plugin, so `Task failed: ` and
/// the sentence, `changed: false`, and no interpreter discovered.
pub(super) fn unreadable_failure(msg: &str) -> Option<TaskResult> {
    msg.starts_with(UNREADABLE).then(|| {
        let mut failed = TaskResult::failed_with(format!("Task failed: {msg}"));
        failed.0.insert("changed".into(), Value::Bool(false));
        failed
            .0
            .insert(crate::executor::RAISED_KEY.into(), Value::Bool(true));
        failed
    })
}
