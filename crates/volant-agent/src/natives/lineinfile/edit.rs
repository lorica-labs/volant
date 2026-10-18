// SPDX-License-Identifier: GPL-3.0-or-later
//! Working out the new lines in memory, and the checks that come before anything is written.

use std::fs;

use serde_json::{Map, Value, json};

use crate::modules::Context;
use crate::natives::common::{
    APPEND, AUTOMATIC, BackupError, FsError, IMMUTABLE, access, backup_local, check, has_flags,
    has_xattrs, parent, set_fs_attributes_diff, validate_runs_as_python,
};
use crate::natives::runtime::{Clock, Stop as Halt};

use super::py_regex::compile;
use super::write::{makedirs, write_changes};
use super::{Request, Stop, fail, message};

/// `main`, then `present` or `absent`, for a request inside the subset.
#[expect(clippy::too_many_lines)]
pub(super) fn edit(
    request: &Request,
    context: &Context,
    clock: Clock,
) -> Result<Map<String, Value>, Stop> {
    let path = request.path;
    if fs::metadata(path).is_ok_and(|meta| meta.is_dir()) {
        return Err(fail(format!("Path {path} is a directory !"), Some(256)));
    }
    if request.present && request.line.is_none() {
        return Err(fail("line is required with state=present".into(), None));
    }
    if !request.present
        && request.regexp.is_none()
        && request.search_string.is_none()
        && request.line.is_none()
    {
        return Err(fail(
            "one of line, search_string, or regexp is required with state=absent".into(),
            None,
        ));
    }
    let exists = match fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_symlink() => return Err("the path is a link".into()),
        Ok(meta) if !meta.is_file() => return Err("the path is not a regular file".into()),
        Ok(_) => true,
        Err(err) if err.raw_os_error() == Some(libc::ENOENT) => false,
        Err(err) => return Err(format!("{path}: {err}").into()),
    };
    if !exists {
        if !request.present {
            let mut result = Map::new();
            result.insert("changed".into(), Value::Bool(false));
            result.insert("msg".into(), "file not present".into());
            return Ok(result);
        }
        if !request.create {
            return Err(fail(
                format!("Destination {path} does not exist !"),
                Some(257),
            ));
        }
    }
    let mut lines = if exists {
        readlines(&fs::read(path).map_err(|err| format!("reading {path}: {err}"))?)
    } else {
        Vec::new()
    };

    let mut result = Map::new();
    let (mut changed, mut msg) = if request.present {
        present(request, &mut lines, clock)?
    } else {
        let found = absent(request, &mut lines, clock)?;
        result.insert("found".into(), found.into());
        let msg = if found > 0 {
            format!("{found} line(s) removed")
        } else {
            String::new()
        };
        (found > 0, msg)
    };

    let mut backup = String::new();
    if changed {
        preflight(request, exists, &context.remote_tmp)?;
        check(clock)?;
        if !exists {
            makedirs(path)?;
        }
        if request.backup && exists {
            backup = match backup_local(path, clock) {
                Ok(name) => name,
                Err(BackupError::Exists(name)) => return Err(format!("{name} exists").into()),
                Err(BackupError::Failed(msg)) => return Err(fail(msg, None)),
                Err(BackupError::Stopped(halt)) => return Err(Stop::Clock(halt)),
            };
        }
        write_changes(request, &lines.concat(), &context.remote_tmp, clock)?;
    }

    let mut attr_diff = Map::new();
    match set_fs_attributes_diff(
        path,
        request.owner.as_ref(),
        request.group.as_ref(),
        request.mode.as_ref(),
        &mut attr_diff,
    ) {
        Ok(true) => {
            if changed {
                msg.push_str(" and ");
            }
            changed = true;
            msg.push_str("ownership, perms or SE linux context changed");
        }
        Ok(false) => {}
        Err(FsError::Failed(fail)) => return Err(Stop::Fail(fail)),
        Err(FsError::Raised(error, touched)) if changed || touched => {
            return Err(Stop::Fail(message(error)));
        }
        Err(FsError::Raised(error, _)) => return Err(Stop::Back(error)),
    }
    attr_diff.insert(
        "before_header".into(),
        format!("{path} (file attributes)").into(),
    );
    attr_diff.insert(
        "after_header".into(),
        format!("{path} (file attributes)").into(),
    );
    // Volant refuses `--diff`, so the content half is always the reference's empty one.
    let diff = json!({
        "before": "",
        "after": "",
        "before_header": format!("{path} (content)"),
        "after_header": format!("{path} (content)"),
    });
    result.insert("changed".into(), Value::Bool(changed));
    result.insert("msg".into(), msg.into());
    result.insert("backup".into(), backup.into());
    result.insert("diff".into(), json!([diff, attr_diff]));
    Ok(result)
}

/// `f.readlines()` on a file opened `rb`: each line keeps its `\n`, the last may have none.
fn readlines(content: &[u8]) -> Vec<Vec<u8>> {
    content
        .split_inclusive(|&byte| byte == b'\n')
        .map(<[u8]>::to_vec)
        .collect()
}

/// `b_cur_line.rstrip(b'\r\n')`.
fn rstrip(line: &[u8]) -> &[u8] {
    let end = line
        .iter()
        .rposition(|&byte| byte != b'\r' && byte != b'\n')
        .map_or(0, |at| at + 1);
    &line[..end]
}

/// Stops a scan of a large file at the task's `timeout` or cancel.
fn tick(lineno: usize, clock: Clock) -> Result<(), Halt> {
    if lineno.is_multiple_of(4096) {
        check(clock)?;
    }
    Ok(())
}

/// `present`: the new lines, whether they changed, and the message.
#[expect(clippy::too_many_lines)]
fn present(
    request: &Request,
    lines: &mut Vec<Vec<u8>>,
    clock: Clock,
) -> Result<(bool, String), Halt> {
    let line = request.line.unwrap_or_default().as_bytes();
    // `main`'s own default, set there rather than in the spec because of `mutually_exclusive`.
    let insertafter = match (request.insertafter, request.insertbefore) {
        (None, None) => Some("EOF"),
        (after, _) => after,
    };
    let insertbefore = request.insertbefore;
    let ins = match (insertafter, insertbefore) {
        (Some(after), _) if !matches!(after, "BOF" | "EOF") => Some(compile("insertafter", after)?),
        (_, Some(before)) if before != "BOF" => Some(compile("insertbefore", before)?),
        _ => None,
    };
    let firstmatch = request.firstmatch;

    // Where the expression or the string matched (`index[0]`), where to insert (`index[1]`).
    let mut found: Option<usize> = None;
    let mut insert_at: Option<usize> = None;
    let mut matched = false;
    if let Some(regexp) = &request.regexp {
        for (lineno, cur) in lines.iter().enumerate() {
            tick(lineno, clock)?;
            if regexp.is_match(cur) {
                found = Some(lineno);
                matched = true;
                if firstmatch {
                    break;
                }
            }
        }
    }
    if let Some(search) = request.search_string {
        for (lineno, cur) in lines.iter().enumerate() {
            tick(lineno, clock)?;
            if cur.windows(search.len()).any(|window| window == search) {
                found = Some(lineno);
                matched = true;
                if firstmatch {
                    break;
                }
            }
        }
    }
    if !matched {
        for (lineno, cur) in lines.iter().enumerate() {
            tick(lineno, clock)?;
            if line == rstrip(cur) {
                found = Some(lineno);
            } else if ins.as_ref().is_some_and(|ins| ins.is_match(cur)) {
                if insertafter.is_some() {
                    insert_at = Some(lineno + 1);
                    if firstmatch {
                        break;
                    }
                }
                if insertbefore.is_some() {
                    insert_at = Some(lineno);
                    if firstmatch {
                        break;
                    }
                }
            }
        }
    }

    let with_sep = || [line, b"\n"].concat();
    let added = || Ok((true, "line added".to_string()));
    let unchanged = || Ok((false, String::new()));
    // The reference's branch for "no regexp, no search_string, no exact line" under `found`
    // cannot be reached: `found` is only ever set by one of the three.
    if let Some(at) = found {
        let new = if line.ends_with(b"\n") {
            line.to_vec()
        } else {
            with_sep()
        };
        if lines[at] != new {
            lines[at] = new;
            return Ok((true, "line replaced".to_string()));
        }
        return unchanged();
    }
    if insertbefore == Some("BOF") || insertafter == Some("BOF") {
        lines.insert(0, with_sep());
        return added();
    }
    let Some(at) = insert_at.filter(|_| insertafter != Some("EOF")) else {
        if lines
            .last()
            .is_some_and(|last| !matches!(last.last(), Some(b'\n' | b'\r')))
        {
            lines.push(b"\n".to_vec());
        }
        lines.push(with_sep());
        return added();
    };
    if insertafter.is_some() {
        if lines.len() == at {
            if rstrip(&lines[at - 1]) != line {
                lines.push(with_sep());
                return added();
            }
        } else if line != rstrip(&lines[at]) {
            lines.insert(at, with_sep());
            return added();
        }
        return unchanged();
    }
    lines.insert(at, with_sep());
    added()
}

/// `absent`: drops every matching line and returns how many it dropped.
fn absent(request: &Request, lines: &mut Vec<Vec<u8>>, clock: Clock) -> Result<usize, Halt> {
    let line = request.line.unwrap_or_default().as_bytes();
    let mut kept = Vec::with_capacity(lines.len());
    let mut found = 0;
    for (lineno, cur) in lines.drain(..).enumerate() {
        tick(lineno, clock)?;
        let hit = match (&request.regexp, request.search_string) {
            (Some(regexp), _) => regexp.is_match(&cur),
            (None, Some(search)) => cur.windows(search.len()).any(|window| window == search),
            (None, None) => line == rstrip(&cur),
        };
        if hit {
            found += 1;
        } else {
            kept.push(cur);
        }
    }
    *lines = kept;
    Ok(found)
}

/// What would hand the write back, checked before its first change.
fn preflight(request: &Request, exists: bool, tmpdir: &str) -> Result<(), String> {
    if !fs::metadata(tmpdir).is_ok_and(|meta| meta.is_dir()) {
        return Err(format!("remote_tmp {tmpdir} is not a directory"));
    }
    // The backup and the rename would fail there, with an exception the Python module words.
    let dir = parent(request.path);
    if fs::metadata(dir).is_ok() && !access(dir, libc::W_OK | libc::X_OK) {
        return Err(format!("{dir} is not writable"));
    }
    // `shutil.copystat` copies them, to the backup and to the new file.
    if exists && has_xattrs(request.path) {
        return Err("the file has extended attributes".into());
    }
    // The rename fails on an immutable or append-only file, with an exception the Python
    // module words; `backup_local` copies the flags `lsattr` shows with `chattr`.
    if exists && has_flags(request.path, IMMUTABLE | APPEND) {
        return Err("the file is immutable or append-only".into());
    }
    if exists && request.backup && has_flags(request.path, !AUTOMATIC) {
        return Err("the file has inode flags".into());
    }
    let Some(validate) = request.validate.filter(|validate| validate.contains("%s")) else {
        return Ok(());
    };
    validate_runs_as_python(validate, &format!("{tmpdir}/tmpxxxxxxxx"), "remote_tmp")
}
