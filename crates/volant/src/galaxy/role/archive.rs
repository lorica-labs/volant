// SPDX-License-Identifier: GPL-3.0-or-later
//! A role's archive: a git checkout packed (`scm_archive_resource`), then any archive unpacked
//! into the roles path (`GalaxyRole.install`).

use std::io::Read as _;
use std::path::{Component, Path, PathBuf};

use serde_json::json;

use super::{Options, Role};
use crate::galaxy::Failure;

/// A git role as a tarball: the clone checked out at the version, archived without `.git`
/// unless `-g` keeps it (`scm_archive_resource`).
pub(super) fn scm_archive(role: &Role, dir: &Path, options: Options) -> Result<PathBuf, Failure> {
    let scm = role.scm.as_deref().unwrap_or_default();
    if scm != "git" {
        return Err(Failure::error(format!(
            "- scm {scm} is not currently supported"
        )));
    }
    let io = |e: std::io::Error| Failure::error(e.to_string());
    let Some(git) = crate::galaxy::git::find("git") else {
        return Err(Failure::error(format!(
            "Could not find/use 'git', it is required to continue with installing '{}'.",
            role.src
        )));
    };
    let git = git.display().to_string();
    let clone = tempfile::tempdir().map_err(io)?;
    // `--` ends the options: a `src` or a `name` that starts with `-` is a repository or a
    // directory, never an option of git's.
    let argv = [&git, "clone", "--", &role.src, &role.name];
    scm_run(&argv, clone.path())?;
    let checkout = clone.path().join(&role.name);
    if let Some(version) = role.version.as_deref().filter(|v| !v.is_empty()) {
        scm_run(&[&git, "checkout", version], &checkout)?;
    }
    let tar = dir.join(format!("{}.tar", role.name));
    let mut builder = tar::Builder::new(std::fs::File::create(&tar).map_err(io)?);
    builder.follow_symlinks(false);
    add_tree(
        &mut builder,
        &checkout,
        &checkout,
        &role.name,
        options.keep_scm_meta,
    )
    .map_err(io)?;
    builder.finish().map_err(io)?;
    Ok(tar)
}

/// One git command of `scm_archive_resource`, failing in its words with git's own stderr.
fn scm_run(argv: &[&str], dir: &Path) -> Result<(), Failure> {
    let out = std::process::Command::new(argv[0])
        .args(&argv[1..])
        .current_dir(dir)
        .stdin(std::process::Stdio::null())
        .output()
        .map_err(|e| Failure::error(format!("when executing {}: {e}", argv.join(" "))))?;
    if out.status.success() {
        return Ok(());
    }
    Err(Failure::error(format!(
        "- command {} failed in directory {} (rc={}) - {}",
        argv.join(" "),
        dir.display(),
        out.status.code().unwrap_or(1),
        String::from_utf8_lossy(&out.stderr)
    )))
}

fn add_tree(
    builder: &mut tar::Builder<std::fs::File>,
    top: &Path,
    dir: &Path,
    prefix: &str,
    keep_git: bool,
) -> std::io::Result<()> {
    let mut entries: Vec<PathBuf> = std::fs::read_dir(dir)?
        .flatten()
        .map(|e| e.path())
        .collect();
    entries.sort();
    for path in entries {
        if !keep_git && path.file_name().is_some_and(|n| n == ".git") {
            continue;
        }
        let name = Path::new(prefix).join(path.strip_prefix(top).unwrap_or(&path));
        if path.is_dir() && !path.is_symlink() {
            add_tree(builder, top, &path, prefix, keep_git)?;
        } else {
            builder.append_path_with_name(&path, name)?;
        }
    }
    Ok(())
}

/// One member of a role's archive.
struct Member {
    name: String,
    kind: tar::EntryType,
    mode: u32,
    bytes: Vec<u8>,
    link: Option<String>,
}

fn members(archive: &Path) -> Result<Vec<Member>, Failure> {
    let invalid =
        || Failure::error("the downloaded file does not appear to be a valid tar archive.");
    let raw = std::fs::read(archive).map_err(|e| Failure::error(e.to_string()))?;
    let data = if raw.starts_with(&[0x1f, 0x8b]) {
        let mut out = Vec::new();
        flate2::read::GzDecoder::new(&raw[..])
            .read_to_end(&mut out)
            .map_err(|_| invalid())?;
        out
    } else {
        raw
    };
    let mut out = Vec::new();
    let mut tar = tar::Archive::new(&data[..]);
    for entry in tar.entries().map_err(|_| invalid())? {
        let mut entry = entry.map_err(|_| invalid())?;
        let mut bytes = Vec::new();
        entry.read_to_end(&mut bytes).map_err(|_| invalid())?;
        out.push(Member {
            name: entry
                .path()
                .map_err(|_| invalid())?
                .to_string_lossy()
                .into_owned(),
            kind: entry.header().entry_type(),
            mode: entry.header().mode().unwrap_or(0o644),
            link: entry
                .link_name()
                .ok()
                .flatten()
                .map(|l| l.to_string_lossy().into_owned()),
            bytes,
        });
    }
    if out.is_empty() {
        return Err(invalid());
    }
    Ok(out)
}

/// The directory that holds the shallowest `meta/main.yml`, the first of equal depth.
fn top_dir(members: &[Member]) -> Option<String> {
    let mut parent: Option<String> = None;
    for member in members {
        if member.name.contains("meta/main.yml") || member.name.contains("meta/main.yaml") {
            let dir = Path::new(&member.name)
                .parent()
                .and_then(Path::parent)
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_default();
            if parent.as_ref().is_none_or(|p| dir.len() < p.len()) {
                parent = Some(dir);
            }
        }
    }
    parent
}

/// Extracts `archive` into the role's directory: the members under the directory that holds
/// the shallowest `meta/main.yml`, that directory dropped; regular files and links only, none
/// leaving the role, with the modes tarfile's `data` filter keeps.
pub(super) fn extract(role: &Role, archive: &Path, force: bool) -> Result<(), Failure> {
    let members = members(archive)?;
    let Some(parent) = top_dir(&members) else {
        return Err(Failure::error(
            "this role does not appear to have a meta/main.yml file.",
        ));
    };
    println!("- extracting {} to {}", role.name, role.path.display());
    let io = |e: std::io::Error| {
        Failure::error(format!(
            "Could not update files in {}: {e}",
            role.path.display()
        ))
    };
    if role.path.exists() {
        if !role.path.is_dir() {
            return Err(Failure::error(
                "the specified roles path exists and is not a directory.",
            ));
        }
        if !force {
            return Err(Failure::error(format!(
                "the specified role {} appears to already exist. Use --force to replace it.",
                role.name
            )));
        }
        if !role.remove() {
            return Err(Failure::error(format!(
                "{} doesn't appear to contain a role.\n  please remove this directory manually if you really want to put the role here.",
                role.path.display()
            )));
        }
    }
    std::fs::create_dir_all(&role.path).map_err(io)?;
    // The reference checks each member against the archive's top directory taken from the
    // working directory, and names that path in its error.
    let resolved = crate::galaxy::resolve(&parent);
    for member in &members {
        let symlink = member.kind == tar::EntryType::Symlink;
        if !(symlink
            || matches!(
                member.kind,
                tar::EntryType::Regular | tar::EntryType::Continuous
            ))
        {
            continue;
        }
        let rel = member.name.replacen(parent.as_str(), "", 1);
        let rel = rel.trim_start_matches('/');
        let full = resolved.join(rel);
        if !folded(&full).starts_with(&resolved) {
            return Err(Failure::error(format!(
                "Invalid name for tarfile member: path {} is not a subpath of the role {}",
                full.display(),
                resolved.display()
            )));
        }
        let target = role.path.join(rel);
        if let Some(dir) = target.parent() {
            std::fs::create_dir_all(dir).map_err(io)?;
        }
        if symlink {
            let link = member.link.clone().unwrap_or_default();
            let full = resolved
                .join(Path::new(rel).parent().unwrap_or(Path::new("")))
                .join(&link);
            if !folded(&full).starts_with(&resolved) {
                return Err(Failure::error(format!(
                    "Invalid linkname for tarfile member: path {} is not a subpath of the role {}",
                    full.display(),
                    resolved.display()
                )));
            }
            #[cfg(unix)]
            std::os::unix::fs::symlink(&link, &target).map_err(io)?;
            continue;
        }
        std::fs::write(&target, &member.bytes).map_err(io)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mut mode = member.mode & 0o755;
            if mode & 0o100 == 0 {
                mode &= !0o111;
            }
            std::fs::set_permissions(&target, std::fs::Permissions::from_mode(mode | 0o600))
                .map_err(io)?;
        }
        #[cfg(not(unix))]
        let _ = member.mode;
    }
    write_install_info(role).map_err(io)
}

/// `meta/.galaxy_install_info`: the UTC install date as `%c` prints it, and the version.
fn write_install_info(role: &Role) -> std::io::Result<()> {
    let meta = role.path.join("meta");
    std::fs::create_dir_all(&meta)?;
    let version = crate::template::inventory_yaml(&crate::template::yaml_node(
        &json!({ "version": role.version }),
    ));
    let date = crate::template::utc_now("%c");
    std::fs::write(
        meta.join(".galaxy_install_info"),
        format!("install_date: '{date}'\n{version}"),
    )
}

/// `path` with `.` and `..` folded out, symbolic links left alone.
fn folded(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for part in path.components() {
        match part {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other),
        }
    }
    out
}
