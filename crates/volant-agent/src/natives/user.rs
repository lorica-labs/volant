// SPDX-License-Identifier: GPL-3.0-or-later
//! `user`, answered in the agent: ansible-core 2.19.12's `user.py`, its generic `User` class that
//! Debian and Ubuntu get, for `name`, `state`, `uid`, `group`, `groups`, `append`, `comment`,
//! `home`, `shell`, `system` and `create_home`. It reads `/etc/passwd` and `/etc/group`, runs
//! the module's `useradd`, `usermod` or `userdel` with the module's arguments in the module's
//! order, and answers with the module's keys, `stdout` and `stderr` included when the command
//! printed them.
//!
//! Handed to the Python module before anything changes: any other argument (`local`,
//! `non_unique`, every `password*`, `expires`, `generate_ssh_key`, every `ssh_key_*`, `remove`,
//! `move_home`, `seuser`, `login_class`, `umask`, `password_lock`, `force` and the rest), a value
//! the module would convert or refuse, `append` without `groups` (the module warns), a group
//! that does not exist (the module fails), a home the module would create itself (a parent that
//! is missing, or a missing home under `create_home`), a host outside Debian and Ubuntu, and an
//! account the name service knows while `/etc/passwd` does not hold it (LDAP, sssd).

use super::{Native, NativeRun};

pub const NATIVE: Native = Native {
    name: "user",
    aliases: &[],
    enabled: cfg!(unix),
    run: linux::run,
};

#[cfg(not(unix))]
mod linux {
    use serde_json::{Map, Value};

    use super::NativeRun;
    use crate::modules::Context;

    pub fn run(_: &Map<String, Value>, _: &Context, _: &dyn Fn() -> bool) -> NativeRun {
        NativeRun::Fallback("the native user changes a Linux host".into())
    }
}

#[cfg(unix)]
mod linux {
    use std::path::{Path, PathBuf};

    use serde_json::{Map, Value};

    use super::NativeRun;
    use crate::modules::Context;
    use crate::natives::common::{
        ArgSpec, Clock, Stop, access, bool_param, check_names, clock, invocation, lookup_user,
        native_run, path_param, str_param, strerror,
    };
    use crate::natives::group::{
        NSSWITCH, bin, entry, failure, host_gate, id_param, outcome, outside_subset, plain_name,
        present, resolve_group, run_command,
    };
    use crate::natives::setup::py_strip;

    pub fn run(
        args: &Map<String, Value>,
        context: &Context,
        cancelled: &dyn Fn() -> bool,
    ) -> NativeRun {
        native_run(
            answer(args, context, clock(context, cancelled), NSSWITCH),
            context,
        )
    }

    const fn arg(name: &'static str, default: fn() -> Value) -> ArgSpec {
        ArgSpec {
            name,
            aliases: &[],
            default,
        }
    }

    /// `user`'s `argument_spec` in ansible-core 2.19.12.
    const SPEC: &[ArgSpec] = &[
        arg("state", || Value::from("present")),
        ArgSpec {
            name: "name",
            aliases: &["user"],
            default: || Value::Null,
        },
        arg("uid", || Value::Null),
        arg("non_unique", || Value::Bool(false)),
        arg("group", || Value::Null),
        arg("groups", || Value::Null),
        arg("comment", || Value::Null),
        arg("home", || Value::Null),
        arg("shell", || Value::Null),
        arg("password", || Value::Null),
        arg("login_class", || Value::Null),
        arg("password_expire_max", || Value::Null),
        arg("password_expire_min", || Value::Null),
        arg("password_expire_warn", || Value::Null),
        arg("hidden", || Value::Null),
        arg("seuser", || Value::Null),
        arg("force", || Value::Bool(false)),
        arg("remove", || Value::Bool(false)),
        ArgSpec {
            name: "create_home",
            aliases: &["createhome"],
            default: || Value::Bool(true),
        },
        arg("skeleton", || Value::Null),
        arg("system", || Value::Bool(false)),
        arg("move_home", || Value::Bool(false)),
        arg("append", || Value::Bool(false)),
        arg("generate_ssh_key", || Value::Null),
        arg("ssh_key_bits", || Value::from(0)),
        arg("ssh_key_type", || Value::from("rsa")),
        arg("ssh_key_file", || Value::Null),
        // `'ansible-generated on %s' % socket.gethostname()`.
        arg("ssh_key_comment", || {
            Value::from(format!("ansible-generated on {}", hostname()))
        }),
        arg("ssh_key_passphrase", || Value::Null),
        arg("update_password", || Value::from("always")),
        arg("expires", || Value::Null),
        arg("password_lock", || Value::Null),
        arg("local", || Value::Null),
        arg("profile", || Value::Null),
        arg("authorization", || Value::Null),
        arg("role", || Value::Null),
        arg("umask", || Value::Null),
        arg("password_expire_account_disable", || Value::Null),
        arg("uid_min", || Value::Null),
        arg("uid_max", || Value::Null),
    ];

    /// The arguments the native answers for; any other one given hands the task back.
    const SUBSET: &[&str] = &[
        "name",
        "user",
        "state",
        "uid",
        "group",
        "groups",
        "append",
        "comment",
        "home",
        "shell",
        "system",
        "create_home",
        "createhome",
    ];

    /// `socket.gethostname()`.
    fn hostname() -> String {
        let mut buf = [0u8; 256];
        if unsafe { libc::gethostname(buf.as_mut_ptr().cast(), buf.len()) } != 0 {
            return String::new();
        }
        let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
        String::from_utf8_lossy(&buf[..end]).into_owned()
    }

    struct Request<'a> {
        name: &'a str,
        present: bool,
        uid: Option<u32>,
        group: Option<&'a str>,
        /// `','.join(groups)`, as the module keeps it and answers it.
        groups: Option<String>,
        append: bool,
        comment: Option<&'a str>,
        home: Option<&'a str>,
        shell: Option<&'a str>,
        system: bool,
        create_home: bool,
    }

    fn request<'a>(
        args: &Map<String, Value>,
        params: &'a Map<String, Value>,
    ) -> Result<Request<'a>, String> {
        check_names(SPEC, args)?;
        outside_subset(args, SUBSET)?;
        let name = str_param(params, "name")?
            .filter(|name| plain_name(name))
            .ok_or("the module reads this account name differently")?;
        // `type='list', elements='str'`, a string already split by `answer`: the module also
        // converts numbers.
        let groups = match &params["groups"] {
            Value::Null => None,
            Value::Array(items) => Some(
                items
                    .iter()
                    .map(|item| {
                        item.as_str()
                            .ok_or("groups holds a value that is not a string")
                    })
                    .collect::<Result<Vec<&str>, _>>()?
                    .join(","),
            ),
            _ => return Err("groups is not a list".into()),
        };
        let path = |key: &str| match &params[key] {
            Value::Null => Ok(None),
            _ => path_param(params, key).map(Some),
        };
        let append = bool_param(params, "append")?;
        if append && groups.is_none() {
            return Err("append without groups makes the module warn".into());
        }
        Ok(Request {
            name,
            present: present(params)?,
            uid: id_param(params, "uid")?,
            group: str_param(params, "group")?,
            groups,
            append,
            comment: str_param(params, "comment")?,
            home: path("home")?,
            shell: path("shell")?,
            system: bool_param(params, "system")?,
            create_home: bool_param(params, "create_home")?,
        })
    }

    /// The fields of a `pwd.getpwnam` entry the module reads.
    struct Account {
        uid: u32,
        gid: u32,
        comment: String,
        home: String,
        shell: String,
    }

    fn passwd_entry(name: &str) -> Result<Option<Account>, String> {
        Ok(entry("/etc/passwd", 7, name)?.and_then(|fields| {
            Some(Account {
                uid: fields[2].parse().ok()?,
                gid: fields[3].parse().ok()?,
                comment: fields[4].clone(),
                home: fields[5].clone(),
                shell: fields[6].clone(),
            })
        }))
    }

    /// The account named `name` as the module finds it through `pwd`, from `/etc/passwd`: `None`
    /// when the host has no such account. An account only the name service knows hands back:
    /// the commands would change the files, not the directory.
    fn find_user(name: &str, clock: Clock) -> Result<Option<Account>, Stop> {
        let known = lookup_user(name, clock)?;
        let local = passwd_entry(name)?;
        match (known, local) {
            (None, None) => Ok(None),
            (Some(known), Some(local)) if known.uid == local.uid => Ok(Some(local)),
            (Some(_), None) => {
                Err("the name service knows this account and /etc/passwd does not".into())
            }
            _ => Err("/etc/passwd and the name service disagree about this account".into()),
        }
    }

    /// `os.path.dirname`.
    fn dirname(path: &str) -> &str {
        let head = &path[..path.rfind('/').map_or(0, |at| at + 1)];
        if head.bytes().any(|b| b != b'/') {
            head.trim_end_matches('/')
        } else {
            head
        }
    }

    /// `set(x.strip() for x in groups.split(',') if x)`, in the order given. The module's set
    /// has no order of its own: Python's string hashing changes it from run to run.
    fn listed(groups: &str) -> Vec<&str> {
        let mut names = Vec::new();
        for name in groups.split(',').filter(|x| !x.is_empty()).map(py_strip) {
            if !names.contains(&name) {
                names.push(name);
            }
        }
        names
    }

    fn no_group() -> Stop {
        Stop::HandBack("a group given does not exist, which the module fails on".into())
    }

    /// `useradd`'s arguments, `create_user_useradd` for the arguments the native takes.
    fn create(
        r: &Request,
        context: &Context,
        clock: Clock,
    ) -> Result<(PathBuf, Vec<String>), Stop> {
        if let Some(home) = r.home
            && r.create_home
            && !Path::new(dirname(home)).is_dir()
        {
            return Err("the module creates the home's missing parent itself".into());
        }
        let useradd = bin(context, "useradd")?;
        let mut argv = Vec::new();
        if let Some(uid) = r.uid {
            argv.extend(["-u".to_string(), uid.to_string()]);
        }
        if let Some(group) = r.group {
            resolve_group(group, clock)?.ok_or_else(no_group)?;
            argv.extend(["-g".to_string(), group.to_string()]);
        } else if resolve_group(r.name, clock)?.is_some() {
            // A group of the account's name exists: no user group.
            argv.push("-N".into());
        }
        if let Some(groups) = r.groups.as_deref().filter(|groups| !groups.is_empty()) {
            let names = listed(groups);
            for name in &names {
                resolve_group(name, clock)?.ok_or_else(no_group)?;
            }
            argv.extend(["-G".to_string(), names.join(",")]);
        }
        if let Some(comment) = r.comment {
            argv.extend(["-c".to_string(), comment.to_string()]);
        }
        if let Some(home) = r.home {
            argv.extend(["-d".to_string(), home.to_string()]);
        }
        if let Some(shell) = r.shell {
            argv.extend(["-s".to_string(), shell.to_string()]);
        }
        argv.push(if r.create_home { "-m" } else { "-M" }.into());
        if r.system {
            argv.push("-r".into());
        }
        argv.push(r.name.to_string());
        Ok((useradd, argv))
    }

    /// The groups whose members name the account, over the whole name service as `grp.getgrall()`
    /// lists them.
    fn memberships(context: &Context, clock: Clock, name: &str) -> Result<Vec<String>, Stop> {
        let (rc, out, _) = run_command(context, clock, &bin(context, "getent")?, &["group"])?;
        if rc != 0 {
            return Err(Stop::HandBack(format!("getent group exited {rc}")));
        }
        Ok(out
            .lines()
            .filter_map(|line| {
                let parts: Vec<&str> = line.split(':').collect();
                (parts.len() >= 4 && parts[3].split(',').any(|member| member == name))
                    .then(|| parts[0].to_string())
            })
            .collect())
    }

    /// `_check_usermod_append`: whether this `usermod` lists `-a, --append` in its help.
    fn has_append(context: &Context, clock: Clock, usermod: &Path) -> Result<bool, Stop> {
        if !usermod
            .to_str()
            .is_some_and(|path| access(path, libc::X_OK))
        {
            return Ok(false);
        }
        let (_, out, err) = run_command(context, clock, usermod, &["--help"])?;
        Ok(format!("{out}{err}")
            .split('\n')
            .any(|line| py_strip(line).starts_with("-a, --append")))
    }

    /// `usermod`'s arguments, `modify_user_usermod` for the arguments the native takes, or `None`
    /// when nothing differs.
    fn modify(
        r: &Request,
        account: &Account,
        context: &Context,
        clock: Clock,
    ) -> Result<Option<(PathBuf, Vec<String>)>, Stop> {
        // The module creates a missing home after `usermod`, under `create_home`.
        if r.create_home && !Path::new(r.home.unwrap_or(&account.home)).exists() {
            return Err("the module creates the missing home itself".into());
        }
        let usermod = bin(context, "usermod")?;
        let mut argv = Vec::new();
        if let Some(uid) = r.uid.filter(|uid| *uid != account.uid) {
            argv.extend(["-u".to_string(), uid.to_string()]);
        }
        if let Some(group) = r.group {
            let group = resolve_group(group, clock)?.ok_or_else(no_group)?;
            if group.gid != account.gid {
                argv.extend(["-g".to_string(), group.gid.to_string()]);
            }
        }
        if let Some(groups) = &r.groups {
            let current = memberships(context, clock, r.name)?;
            let mut wanted: Vec<String> = Vec::new();
            for name in listed(groups) {
                let group = resolve_group(name, clock)?.ok_or_else(no_group)?;
                if !wanted.contains(&group.name) {
                    wanted.push(group.name);
                }
            }
            let added = wanted.iter().any(|group| !current.contains(group));
            let removed = current.iter().any(|group| !wanted.contains(group));
            let change = if r.append {
                if added && !has_append(context, clock, &usermod)? {
                    return Err("this usermod has no --append: the module passes -A".into());
                }
                if added {
                    argv.push("-a".into());
                }
                added
            } else {
                added || removed
            };
            if change {
                argv.extend(["-G".to_string(), wanted.join(",")]);
            }
        }
        if let Some(comment) = r.comment.filter(|comment| *comment != account.comment) {
            argv.extend(["-c".to_string(), comment.to_string()]);
        }
        if let Some(home) = r.home.filter(|home| *home != account.home) {
            argv.extend(["-d".to_string(), home.to_string()]);
        }
        if let Some(shell) = r.shell.filter(|shell| *shell != account.shell) {
            argv.extend(["-s".to_string(), shell.to_string()]);
        }
        if argv.is_empty() {
            return Ok(None);
        }
        argv.push(r.name.to_string());
        Ok(Some((usermod, argv)))
    }

    /// Python's `str()` of the `OSError` an `os` call on the text path `path` raises.
    fn os_error(err: &std::io::Error, path: &Path) -> String {
        let errno = err.raw_os_error().unwrap_or(0);
        let text = strerror(errno).map_or_else(|| err.to_string(), str::to_string);
        format!("[Errno {errno}] {text}: '{}'", path.display())
    }

    /// `create_homedir` then `chown_homedir`, for a home `useradd -m` left missing: the skeleton
    /// copied as `shutil.copytree(symlinks=True)` copies it (or the directory made when there is
    /// no skeleton), its mode from `login.defs`, then the account given the home and everything
    /// in it.
    fn create_home(
        skel: &Path,
        login_defs: &Path,
        home: &Path,
        uid: u32,
        gid: u32,
    ) -> Result<(), String> {
        use std::os::unix::fs::PermissionsExt;
        if skel.exists() {
            copy_tree(skel, home)?;
        } else {
            std::fs::create_dir_all(home).map_err(|err| os_error(&err, home))?;
        }
        if login_defs.exists() {
            let defs =
                std::fs::read_to_string(login_defs).map_err(|err| os_error(&err, login_defs))?;
            // Neither set: `useradd`'s own UMASK of 022.
            let mut mode = 0o755;
            for line in defs.split('\n') {
                if let Some(value) = setting(line, "HOME_MODE") {
                    mode = value?;
                    break;
                }
                if let Some(value) = setting(line, "UMASK") {
                    mode = 0o777 & !value?;
                }
            }
            std::fs::set_permissions(home, std::fs::Permissions::from_mode(mode))
                .map_err(|err| os_error(&err, home))?;
        }
        let chown = |path: &Path| {
            std::os::unix::fs::chown(path, Some(uid), Some(gid)).map_err(|err| os_error(&err, path))
        };
        chown(home)?;
        chown_tree(home, &chown)
    }

    /// `re.match(r'^<name>\s+(\d+)$', line)`, the digits read by `int(_, 8)`.
    fn setting(line: &str, name: &str) -> Option<Result<u32, String>> {
        let rest = line.strip_prefix(name)?;
        let digits = rest.trim_start();
        if digits.len() == rest.len() || digits.is_empty() || !digits.chars().all(char::is_numeric)
        {
            return None;
        }
        Some(
            u32::from_str_radix(digits, 8)
                .map_err(|_| format!("invalid literal for int() with base 8: '{digits}'")),
        )
    }

    /// `shutil.copytree(src, dst, symlinks=True)`: a link copied as a link with its times, a
    /// file with its content, mode and times (`copy2`), a directory's mode and times set once it
    /// is filled. Extended attributes, which `copy2` also copies, are not.
    fn copy_tree(src: &Path, dst: &Path) -> Result<(), String> {
        std::fs::create_dir_all(dst).map_err(|err| os_error(&err, dst))?;
        for entry in std::fs::read_dir(src).map_err(|err| os_error(&err, src))? {
            let entry = entry.map_err(|err| os_error(&err, src))?;
            let (from, to) = (entry.path(), dst.join(entry.file_name()));
            let meta = std::fs::symlink_metadata(&from).map_err(|err| os_error(&err, &from))?;
            if meta.file_type().is_symlink() {
                let target = std::fs::read_link(&from).map_err(|err| os_error(&err, &from))?;
                std::os::unix::fs::symlink(&target, &to).map_err(|err| os_error(&err, &to))?;
                copy_stat(&meta, &to, false)?;
            } else if meta.is_dir() {
                copy_tree(&from, &to)?;
            } else {
                std::fs::copy(&from, &to).map_err(|err| os_error(&err, &from))?;
                copy_stat(&meta, &to, true)?;
            }
        }
        let meta = std::fs::metadata(src).map_err(|err| os_error(&err, src))?;
        copy_stat(&meta, dst, true)
    }

    /// `shutil.copystat` without extended attributes: the mode (not for a link, which Linux
    /// cannot `chmod`), and the access and modification times to the nanosecond.
    fn copy_stat(meta: &std::fs::Metadata, to: &Path, follow: bool) -> Result<(), String> {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        if follow {
            std::fs::set_permissions(to, std::fs::Permissions::from_mode(meta.mode() & 0o7777))
                .map_err(|err| os_error(&err, to))?;
        }
        let times = [
            libc::timespec {
                tv_sec: meta.atime(),
                tv_nsec: meta.atime_nsec(),
            },
            libc::timespec {
                tv_sec: meta.mtime(),
                tv_nsec: meta.mtime_nsec(),
            },
        ];
        let flags = if follow { 0 } else { libc::AT_SYMLINK_NOFOLLOW };
        let path = std::ffi::CString::new(to.as_os_str().as_encoded_bytes())
            .map_err(|_| format!("{} holds a NUL byte", to.display()))?;
        if unsafe { libc::utimensat(libc::AT_FDCWD, path.as_ptr(), times.as_ptr(), flags) } != 0 {
            return Err(os_error(&std::io::Error::last_os_error(), to));
        }
        Ok(())
    }

    /// The loop of `chown_homedir` over `os.walk(home)`: every directory, a link to one
    /// included (and followed), and every file that is not a link. A directory that cannot be
    /// listed is skipped, as `os.walk` skips it.
    fn chown_tree(dir: &Path, chown: &dyn Fn(&Path) -> Result<(), String>) -> Result<(), String> {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return Ok(());
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let link = entry.file_type().is_ok_and(|kind| kind.is_symlink());
            if std::fs::metadata(&path).is_ok_and(|meta| meta.is_dir()) {
                chown(&path)?;
                if !link {
                    chown_tree(&path, chown)?;
                }
            } else if !link {
                chown(&path)?;
            }
        }
        Ok(())
    }

    fn answer(
        args: &Map<String, Value>,
        context: &Context,
        clock: Clock,
        nsswitch: &str,
    ) -> Result<Map<String, Value>, Stop> {
        let mut invocation = invocation(SPEC, args);
        // `type='list'` splits a string on commas, and `invocation` shows the list.
        if let Some(Value::String(groups)) = invocation["module_args"].get("groups").cloned() {
            invocation["module_args"]["groups"] =
                Value::from(groups.split(',').collect::<Vec<_>>());
        }
        let params = invocation["module_args"].as_object().unwrap();
        let r = request(args, params)?;
        host_gate(nsswitch)?;
        for file in ["/etc/redhat-release", "/etc/SuSE-release"] {
            if Path::new(file).exists() {
                return Err(format!("{file} changes the module's useradd arguments").into());
            }
        }
        let found = find_user(r.name, clock)?;
        // Decided, and every command found, before anything runs.
        let command = match (r.present, &found) {
            (false, None) => None,
            (false, Some(_)) => Some((bin(context, "userdel")?, vec![r.name.to_string()])),
            (true, None) => Some(create(&r, context, clock)?),
            (true, Some(account)) => modify(&r, account, context, clock)?,
        };
        let ran = match command {
            Some((program, argv)) => {
                let argv: Vec<&str> = argv.iter().map(String::as_str).collect();
                let ran = run_command(context, clock, &program, &argv)?;
                if ran.0 != 0 {
                    return Ok(failure(
                        invocation.clone(),
                        &[
                            ("name", Value::from(r.name)),
                            ("msg", Value::from(ran.2)),
                            ("rc", Value::from(ran.0)),
                        ],
                    ));
                }
                Some(ran)
            }
            None => None,
        };
        let mut result = Map::new();
        result.insert("name".into(), Value::from(r.name));
        result.insert("state".into(), params["state"].clone());
        match (r.present, found.is_some()) {
            (false, true) => {
                result.insert("force".into(), params["force"].clone());
                result.insert("remove".into(), params["remove"].clone());
            }
            (true, false) => {
                result.insert("system".into(), Value::Bool(r.system));
                result.insert("create_home".into(), Value::Bool(r.create_home));
            }
            (true, true) => {
                result.insert("append".into(), Value::Bool(r.append));
                result.insert("move_home".into(), params["move_home"].clone());
            }
            (false, false) => {}
        }
        outcome(&mut result, ran.as_ref());
        if r.present {
            match passwd_entry(r.name) {
                Ok(Some(account)) => {
                    let home = r.home.unwrap_or(&account.home).to_string();
                    result.insert("uid".into(), Value::from(account.uid));
                    result.insert("group".into(), Value::from(account.gid));
                    result.insert("comment".into(), Value::from(account.comment));
                    result.insert("home".into(), Value::from(account.home));
                    result.insert("shell".into(), Value::from(account.shell));
                    if let Some(groups) = &r.groups {
                        result.insert("groups".into(), Value::from(groups.as_str()));
                    }
                    // Ruled out before a `usermod`, which hands back; after `useradd -m`, which
                    // creates the home or fails, the module's own creation.
                    if r.create_home && !Path::new(&home).exists() {
                        let made = create_home(
                            Path::new("/etc/skel"),
                            Path::new("/etc/login.defs"),
                            Path::new(&home),
                            account.uid,
                            account.gid,
                        );
                        if let Err(msg) = made {
                            return Ok(failure(invocation.clone(), &[("msg", Value::from(msg))]));
                        }
                        result.insert("changed".into(), Value::Bool(true));
                    }
                }
                Ok(None) => {}
                Err(reason) => {
                    return Ok(failure(invocation.clone(), &[("msg", Value::from(reason))]));
                }
            }
        }
        result.insert("invocation".into(), invocation.clone());
        Ok(result)
    }

    #[cfg(all(test, target_os = "linux"))]
    mod tests {
        use serde_json::json;

        use super::*;
        use crate::natives::common::golden::fake_getent;
        use crate::natives::group::tests::{Fakes, args, scratch_name};
        use crate::natives::setup::unbounded;

        fn ask(args: &Map<String, Value>, context: &Context) -> Result<Map<String, Value>, Stop> {
            answer(args, context, unbounded(), NSSWITCH)
        }

        /// An account the host does not have is created with the module's `useradd` arguments
        /// in the module's order, and answered with the module's keys.
        ///
        /// What would make this red: any two options swapped, or the name anywhere but last;
        /// `-N` missing where a group of the account's name exists; `stdout`/`stderr` left out;
        /// account keys answered for an account the host still does not hold.
        #[test]
        fn useradd_takes_the_module_s_arguments_in_its_order() {
            let fakes = Fakes::new("useradd", &["useradd"], 0);
            if fakes.outside(ask) {
                return;
            }
            let name = scratch_name("volantu");
            let mut created = ask(
                &args(json!({
                    "name": name, "uid": 64998, "group": "root", "groups": "root",
                    "comment": "c", "home": "/nonexistent-volant", "shell": "/bin/sh",
                    "create_home": false, "system": true,
                })),
                &fakes.context,
            )
            .unwrap();
            assert_eq!(
                fakes.argv("useradd").unwrap(),
                [
                    "-u",
                    "64998",
                    "-g",
                    "root",
                    "-G",
                    "root",
                    "-c",
                    "c",
                    "-d",
                    "/nonexistent-volant",
                    "-s",
                    "/bin/sh",
                    "-M",
                    "-r",
                    name.as_str(),
                ]
            );
            // A string `groups` is split, and shown as the list.
            assert_eq!(
                created["invocation"]["module_args"]["groups"],
                json!(["root"])
            );
            created.remove("invocation");
            assert_eq!(
                Value::Object(created),
                json!({"name": name, "state": "present", "changed": true, "system": true,
                       "create_home": false, "stdout": "out\n", "stderr": "err\n"})
            );

            // `users` is a group on Debian and Ubuntu, and no account.
            ask(&args(json!({"name": "users"})), &fakes.context).unwrap();
            assert_eq!(fakes.argv("useradd").unwrap(), ["-N", "-m", "users"]);
        }

        /// An existing account gets `usermod` with only what differs, in the module's order:
        /// the uid, the primary group by gid, the groups, the comment, the home, the shell.
        ///
        /// What would make this red: an option passed for a value already right, two options
        /// swapped, the group passed by name, or `-a` missing under `append`.
        #[test]
        fn usermod_takes_only_what_differs_in_the_module_s_order() {
            let fakes = Fakes::new("usermod", &["usermod"], 0);
            if fakes.outside(ask) {
                return;
            }
            // The account is in no group's member list.
            fakes.script("getent", "[ \"$1\" = group ] && echo 'root:x:0:'");
            let mut same = ask(
                &args(json!({"name": "root", "uid": 0, "group": "root", "home": "/root", "create_home": false})),
                &fakes.context,
            )
            .unwrap();
            assert_eq!(fakes.argv("usermod"), None);
            same.remove("invocation");
            let shell = same["shell"].clone();
            assert_eq!(
                Value::Object(same),
                json!({"name": "root", "state": "present", "changed": false, "append": false,
                       "move_home": false, "uid": 0, "group": 0, "comment": "root",
                       "home": "/root", "shell": shell})
            );

            let changed = ask(
                &args(json!({
                    "name": "root", "uid": 1, "group": "daemon", "groups": ["root"],
                    "comment": "c", "home": "/root", "shell": "/bin/volant-none",
                    "create_home": false,
                })),
                &fakes.context,
            )
            .unwrap();
            assert_eq!(
                fakes.argv("usermod").unwrap(),
                [
                    "-u",
                    "1",
                    "-g",
                    "1",
                    "-G",
                    "root",
                    "-c",
                    "c",
                    "-s",
                    "/bin/volant-none",
                    "root"
                ]
            );
            assert_eq!(changed["changed"], true);
            assert_eq!(changed["groups"], "root");

            ask(
                &args(json!({"name": "root", "groups": ["root"], "append": true, "create_home": false})),
                &fakes.context,
            )
            .unwrap();
            assert_eq!(fakes.argv("usermod").unwrap(), ["-a", "-G", "root", "root"]);
        }

        /// The membership cases where the account already belongs to groups: without `append`,
        /// a group given fewer is `-G` with those left, and `[]` is `-G ""`; with `append`, a
        /// difference that is only a removal runs nothing, nor does `[]`.
        ///
        /// What would make this red: a removal ignored without `append`, or acted on with it;
        /// `[]` read as "no change" without `append`, or as a clearing with it.
        #[test]
        fn a_membership_only_removed_follows_append() {
            let fakes = Fakes::new("usermod-members", &["usermod"], 0);
            if fakes.outside(ask) {
                return;
            }
            fakes.script(
                "getent",
                "[ \"$1\" = group ] && printf 'root:x:0:root\\ndaemon:x:1:bin,root\\n'",
            );
            for (groups, append, argv) in [
                (json!(["root"]), false, Some(vec!["-G", "root", "root"])),
                (json!([]), false, Some(vec!["-G", "", "root"])),
                (json!(["root"]), true, None),
                (json!([]), true, None),
            ] {
                fakes.clear("usermod");
                let given = json!({"name": "root", "groups": groups, "append": append, "create_home": false});
                let answer = ask(&args(given), &fakes.context).unwrap();
                assert_eq!(
                    fakes.argv("usermod"),
                    argv.clone()
                        .map(|argv| argv.into_iter().map(String::from).collect()),
                    "groups {groups}, append {append}"
                );
                assert_eq!(answer["changed"], argv.is_some());
            }
        }

        /// `absent` runs `userdel <name>` for an account that exists and nothing otherwise.
        ///
        /// What would make this red: `userdel` given more than the name, `force` and `remove`
        /// missing from a removal, or present for an account that was never there.
        #[test]
        fn userdel_runs_only_for_an_existing_account() {
            let fakes = Fakes::new("userdel", &["userdel"], 0);
            if fakes.outside(ask) {
                return;
            }
            let mut gone = ask(
                &args(json!({"name": "root", "state": "absent"})),
                &fakes.context,
            )
            .unwrap();
            assert_eq!(fakes.argv("userdel").unwrap(), ["root"]);
            gone.remove("invocation");
            assert_eq!(
                Value::Object(gone),
                json!({"name": "root", "state": "absent", "changed": true, "force": false,
                       "remove": false, "stdout": "out\n", "stderr": "err\n"})
            );

            let name = scratch_name("volantm");
            let mut missing = ask(
                &args(json!({"name": name, "state": "absent"})),
                &fakes.context,
            )
            .unwrap();
            missing.remove("invocation");
            assert_eq!(
                Value::Object(missing),
                json!({"name": name, "state": "absent", "changed": false})
            );
        }

        /// A command that fails is the module's failure: `name`, the error output as `msg`, `rc`,
        /// and nothing else.
        ///
        /// What would make this red: a failure answered as a change, with the outputs, or
        /// without `rc`.
        #[test]
        fn a_failing_useradd_is_the_module_s_failure() {
            let fakes = Fakes::new("useradd-fails", &["useradd"], 9);
            if fakes.outside(ask) {
                return;
            }
            let name = scratch_name("volantf");
            let mut answer = ask(
                &args(json!({"name": name, "create_home": false})),
                &fakes.context,
            )
            .unwrap();
            assert!(answer.remove("invocation").is_some());
            assert_eq!(
                Value::Object(answer),
                json!({"failed": true, "name": name, "msg": "err\n", "rc": 9})
            );
        }

        /// Every argument outside the subset hands back before any command runs.
        ///
        /// What would make this red: one of them answered, `remove` or `password` above all,
        /// or a command run before handing back.
        #[test]
        fn an_argument_outside_the_subset_hands_back_before_any_command() {
            let fakes = Fakes::new("user-subset", &["useradd", "usermod", "userdel"], 0);
            let name = scratch_name("volants");
            for (key, value) in [
                ("local", json!(true)),
                ("non_unique", json!(true)),
                ("password", json!("!")),
                ("password_expire_max", json!(1)),
                ("password_expire_min", json!(1)),
                ("password_expire_warn", json!(1)),
                ("password_expire_account_disable", json!(1)),
                ("password_lock", json!(true)),
                ("update_password", json!("on_create")),
                ("expires", json!(1.0)),
                ("generate_ssh_key", json!(true)),
                ("ssh_key_bits", json!(2048)),
                ("ssh_key_type", json!("ed25519")),
                ("ssh_key_file", json!("/x")),
                ("ssh_key_comment", json!("c")),
                ("ssh_key_passphrase", json!("p")),
                ("remove", json!(true)),
                ("move_home", json!(true)),
                ("seuser", json!("u")),
                ("login_class", json!("c")),
                ("umask", json!("022")),
                ("force", json!(true)),
                ("skeleton", json!("/etc/skel")),
                ("hidden", json!(true)),
                ("profile", json!("p")),
                ("authorization", json!("a")),
                ("role", json!("r")),
                ("uid_min", json!(1000)),
                ("uid_max", json!(2000)),
            ] {
                for state in ["present", "absent"] {
                    for who in [name.as_str(), "root"] {
                        let mut given =
                            args(json!({"name": who, "state": state, "create_home": false}));
                        given.insert(key.into(), value.clone());
                        let answer = ask(&given, &fakes.context);
                        assert!(
                            matches!(answer, Err(Stop::HandBack(_))),
                            "{key} was answered"
                        );
                    }
                }
            }
            for command in ["useradd", "usermod", "userdel"] {
                assert_eq!(fakes.argv(command), None, "{command} ran");
            }
        }

        /// An account `/etc/passwd` does not hold and the name service knows is handed back,
        /// before `useradd` or `userdel` runs.
        ///
        /// What would make this red: the name service not asked, which reads the account as
        /// missing and runs `useradd` for an account the directory already has.
        #[test]
        fn an_account_only_the_name_service_knows_hands_back() {
            let fakes = Fakes::new("user-nss", &["useradd", "userdel"], 0);
            if fakes.outside(ask) {
                return;
            }
            let name = scratch_name("volantn");
            let _path = fake_getent(
                &fakes.scratch,
                &format!(
                    "[ \"$1\" = passwd ] && [ \"$2\" = {name} ] && echo '{name}:x:5000:5000::/home/{name}:/bin/sh' && exit 0; exit 2"
                ),
            );
            for state in ["present", "absent"] {
                let answer = ask(
                    &args(json!({"name": name, "state": state, "create_home": false})),
                    &fakes.context,
                );
                assert!(
                    matches!(answer, Err(Stop::HandBack(_))),
                    "{state} was answered"
                );
            }
            assert_eq!(fakes.argv("useradd"), None);
            assert_eq!(fakes.argv("userdel"), None);
        }

        /// A home the module would create itself hands back before any command: a missing
        /// parent before `useradd`, a missing home under `create_home` before `usermod`.
        ///
        /// What would make this red: `useradd -m` run where the module makes the parent first,
        /// or `usermod` run where the module then creates the home and answers `changed`.
        #[test]
        fn a_home_the_module_creates_itself_hands_back() {
            let fakes = Fakes::new("user-home", &["useradd", "usermod"], 0);
            if fakes.outside(ask) {
                return;
            }
            let missing = format!("/nonexistent-volant-{}", std::process::id());
            let name = scratch_name("volanth");
            let parent = ask(
                &args(json!({"name": name, "home": format!("{missing}/home")})),
                &fakes.context,
            );
            assert!(matches!(parent, Err(Stop::HandBack(_))));
            let home = ask(
                &args(json!({"name": "root", "home": missing})),
                &fakes.context,
            );
            assert!(matches!(home, Err(Stop::HandBack(_))));
            assert_eq!(fakes.argv("useradd"), None);
            assert_eq!(fakes.argv("usermod"), None);
        }

        /// A home `useradd -m` left missing is made as `create_homedir` makes it: the skeleton
        /// copied with its links, modes and times, the mode `login.defs` gives (`HOME_MODE` over
        /// `UMASK`), and every entry given to the account.
        ///
        /// What would make this red: a link copied as its target, a file's mode or time lost, the
        /// mode from `UMASK` where `HOME_MODE` is set, or a mode other than 0755 without either.
        #[test]
        fn a_missing_home_is_made_from_the_skeleton() {
            use std::os::unix::fs::{MetadataExt, PermissionsExt};
            let fakes = Fakes::new("user-skel", &[], 0);
            let skel = fakes.scratch.path("skel");
            std::fs::create_dir_all(format!("{skel}/sub")).unwrap();
            std::fs::write(format!("{skel}/.profile"), "p\n").unwrap();
            std::fs::set_permissions(
                format!("{skel}/.profile"),
                std::fs::Permissions::from_mode(0o600),
            )
            .unwrap();
            std::os::unix::fs::symlink(".profile", format!("{skel}/link")).unwrap();
            std::fs::set_permissions(
                format!("{skel}/sub"),
                std::fs::Permissions::from_mode(0o700),
            )
            .unwrap();
            let (uid, gid) = unsafe { (libc::geteuid(), libc::getegid()) };
            let defs = fakes.scratch.path("login.defs");
            for (text, mode) in [
                ("HOME_MODE\t0750\nUMASK\t\t077\n", 0o750),
                ("UMASK 077\n", 0o700),
                ("# HOME_MODE 0700\nUMASK 022 \n", 0o755),
            ] {
                let home = fakes.scratch.path("home");
                let _ = std::fs::remove_dir_all(&home);
                std::fs::write(&defs, text).unwrap();
                create_home(
                    Path::new(&skel),
                    Path::new(&defs),
                    Path::new(&home),
                    uid,
                    gid,
                )
                .unwrap();
                let meta = std::fs::metadata(&home).unwrap();
                assert_eq!(meta.mode() & 0o7777, mode, "{text:?}");
                assert_eq!(meta.uid(), uid);
                let profile = std::fs::metadata(format!("{home}/.profile")).unwrap();
                let original = std::fs::metadata(format!("{skel}/.profile")).unwrap();
                assert_eq!(profile.mode() & 0o7777, 0o600);
                assert_eq!(
                    (profile.mtime(), profile.mtime_nsec()),
                    (original.mtime(), original.mtime_nsec())
                );
                assert_eq!(
                    std::fs::read_link(format!("{home}/link")).unwrap(),
                    Path::new(".profile")
                );
                let sub = std::fs::metadata(format!("{home}/sub")).unwrap();
                assert_eq!(sub.mode() & 0o7777, 0o700);
            }
            let bare = fakes.scratch.path("bare");
            let no_skel = fakes.scratch.path("no-skel");
            let no_defs = fakes.scratch.path("no-defs");
            create_home(
                Path::new(&no_skel),
                Path::new(&no_defs),
                Path::new(&bare),
                uid,
                gid,
            )
            .unwrap();
            assert!(Path::new(&bare).is_dir());
            assert!(setting("HOME_MODE 0789", "HOME_MODE").unwrap().is_err());
            assert!(setting("HOME_MODES 0700", "HOME_MODE").is_none());
        }

        /// `invocation` holds every argument of the module with its default, as recorded.
        ///
        /// What would make this red: an argument missing, a default that differs, or the
        /// `ssh_key_comment` default without this host's name.
        #[test]
        fn the_invocation_is_the_module_s() {
            let recorded: Value = serde_json::from_str(include_str!(
                "../../../volant/tests/golden/native/user-absent-missing.json"
            ))
            .unwrap();
            let mut want = recorded["invocation"].clone();
            want["module_args"]["ssh_key_comment"] =
                Value::from(format!("ansible-generated on {}", hostname()));
            assert_eq!(
                invocation(
                    SPEC,
                    &args(json!({"name": "volantshape", "state": "absent"}))
                ),
                want
            );
            assert_eq!(dirname("/a/b"), "/a");
            assert_eq!(dirname("/a"), "/");
            assert_eq!(dirname("//a//b"), "//a");
            assert_eq!(listed("a, b,,a"), ["a", "b"]);
        }
    }
}
