// SPDX-License-Identifier: GPL-3.0-or-later
//! Where a task's controller file is found, and how its bytes travel to the host.

use std::path::{Component, Path, PathBuf};

use serde_json::Value;
use volant_protocol::TaskResult;
use volant_protocol::encoding::{b64_encode, sha1_hex, sha1_hex_of};

use std::collections::BTreeSet;

use super::{BlobBody, FileBlob};
use crate::compile::Origin;
use crate::config::HostNamedPaths;

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

/// Past this many bytes a controller file stays on disk: hashed and sent a piece at a time
/// rather than read whole, so the memory of a transfer does not grow with the file.
pub(crate) const ON_DISK_LEN: u64 = 16 * 1024 * 1024;

/// What a task sends to the host: bytes in hand, or a controller file read as it goes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Payload {
    Bytes(Vec<u8>),
    Disk(PathBuf),
}

impl Payload {
    /// The controller file at `path`, read whole up to [`ON_DISK_LEN`] and left on disk past it.
    /// A vaulted file is read whole whatever its length, for its caller to decrypt.
    pub(crate) fn read(path: &Path) -> std::io::Result<Payload> {
        use std::io::Read;
        let len = std::fs::metadata(path)?.len();
        if len > ON_DISK_LEN {
            let mut head = Vec::new();
            std::fs::File::open(path)?.take(64).read_to_end(&mut head)?;
            if !crate::vault::is_encrypted(&head) {
                return Ok(Payload::Disk(path.to_path_buf()));
            }
        }
        std::fs::read(path).map(Payload::Bytes)
    }

    /// The SHA-1 of the bytes, the sum the host's `stat` reports.
    pub(crate) fn sha1(&self) -> std::io::Result<String> {
        match self {
            Payload::Bytes(bytes) => Ok(sha1_hex(bytes)),
            Payload::Disk(path) => sha1_hex_of(std::fs::File::open(path)?),
        }
    }

    /// The bytes as a blob, named by their blake3 as the union is: the agent refuses a blob
    /// whose bytes hash to anything else, a file that changed after it was hashed included.
    pub(crate) fn blob(&self) -> std::io::Result<FileBlob> {
        Ok(match self {
            Payload::Bytes(bytes) => FileBlob {
                hash: blake3::hash(bytes).to_hex().to_string(),
                body: BlobBody::B64(b64_encode(bytes)),
            },
            Payload::Disk(path) => {
                let mut hasher = blake3::Hasher::new();
                hasher.update_reader(std::fs::File::open(path)?)?;
                FileBlob {
                    hash: hasher.finalize().to_hex().to_string(),
                    body: BlobBody::Disk(path.clone()),
                }
            }
        })
    }
}

/// `mode: preserve` read off a controller file, as the reference writes it: `0%03o`.
#[cfg(unix)]
pub(crate) fn local_mode(path: &Path) -> Result<String, String> {
    use std::os::unix::fs::PermissionsExt;
    let mode = std::fs::metadata(path)
        .map_err(|err| format!("could not read the mode of {}: {err}", path.display()))?
        .permissions()
        .mode();
    Ok(format!("0{:03o}", mode & 0o7777))
}

#[cfg(not(unix))]
pub(crate) fn local_mode(path: &Path) -> Result<String, String> {
    Err(format!(
        "mode: preserve reads a unix mode, and {} has none on this controller",
        path.display()
    ))
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
