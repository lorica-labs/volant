// SPDX-License-Identifier: GPL-3.0-or-later
//! The `-t` output: the files of the `tree` callback of ansible-core 2.19.12.

use super::Renderer;
use super::recap_json::{ansible_json, ansible_json_indented};

impl Renderer {
    /// The `tree` callback's file for `host`: the result as JSON with no line break at its end,
    /// over whatever the last result left. `event` names the callback method, for the warning a
    /// directory that cannot be made becomes. `pretty` is the result's own [`VERBOSE_ALWAYS`](super::VERBOSE_ALWAYS).
    pub(super) fn tree_file(
        &mut self,
        host: &str,
        event: &str,
        body: serde_json::Map<String, serde_json::Value>,
        pretty: bool,
    ) {
        let Some(dir) = self.tree.clone() else {
            return;
        };
        let value = serde_json::Value::Object(body);
        let text = if pretty || self.verbosity > 2 {
            ansible_json_indented(&value, 0)
        } else {
            ansible_json(&value)
        };
        let repr = |text: &str| crate::playbook::python_repr(&serde_json::Value::from(text));
        // `makedirs_safe` lets a directory that exists be, and so does a file in its place: the
        // write below says what is wrong with that.
        if let Err(err) = std::fs::create_dir_all(&dir)
            && err.kind() != std::io::ErrorKind::AlreadyExists
        {
            let shown = repr(&dir.display().to_string());
            self.warning(
                &format!(
                    "Callback dispatch '{event}' failed for plugin 'tree': Unable to create local directories {shown}: {}: b{shown}",
                    errno_text(&err),
                ),
                false,
            );
            return;
        }
        // The reference joins the name as it is, so `../x` and `/abs` write outside the
        // directory. This release writes only a file named by one plain path component.
        let plain = !host.is_empty() && host != "." && host != ".." && !host.contains(['/', '\0']);
        let path = dir.join(host);
        let written = if plain {
            write_replacing(&dir, &path, &text)
        } else {
            Err(std::io::Error::other("the name is not a single file name"))
        };
        if let Err(err) = written {
            let why = if plain {
                format!(
                    "{}: b{}",
                    errno_text(&err),
                    repr(&path.display().to_string())
                )
            } else {
                err.to_string()
            };
            self.warning(
                &format!("Unable to write to {}'s file: {why}", repr(host)),
                false,
            );
        }
    }
}

/// `text` in a new private file of `dir`, renamed over `path`: a link planted at `path` is
/// replaced and never followed.
fn write_replacing(
    dir: &std::path::Path,
    path: &std::path::Path,
    text: &str,
) -> std::io::Result<()> {
    use std::io::Write as _;
    static COUNT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let n = COUNT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let temp = dir.join(format!(".volant-tree-{}-{n}", std::process::id()));
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    let result = options
        .open(&temp)
        .and_then(|mut file| file.write_all(text.as_bytes()))
        .and_then(|()| std::fs::rename(&temp, path));
    if result.is_err() {
        let _ = std::fs::remove_file(&temp);
    }
    result
}

/// `[Errno 2] No such file or directory`, as Python words an `OSError`.
fn errno_text(err: &std::io::Error) -> String {
    let text = err.to_string();
    match err.raw_os_error() {
        Some(code) => format!(
            "[Errno {code}] {}",
            text.strip_suffix(&format!(" (os error {code})"))
                .unwrap_or(&text)
        ),
        None => text,
    }
}
