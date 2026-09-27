// SPDX-License-Identifier: GPL-3.0-or-later
//! `copy`, answered in the agent as `ansible/modules/copy.py` of ansible-core 2.19.12 answers the
//! `copy` action plugin: `src` is the file the agent staged for the task, and the module moves it
//! over `dest` when the two differ.
//!
//! The native reads first: the source's sums, `checksum`, where `dest` really is, whether it
//! exists and what it holds, and everything that would make the move take the reference's
//! workarounds. Only then does it act, in the reference's order: backup, a link replaced by an
//! empty file, `validate` on the source, then `atomic_move`, which gives the source the owner,
//! mode and a fresh time of the file it replaces and renames it into place. Owner, group and mode
//! come last. Every hand-back comes before the backup. Outside the subset the Python module
//! answers: `remote_src`, `directory_mode`, `content`, SELinux, `attributes`, a destination with
//! extended attributes or an ACL, a source on another mount than the destination (the reference
//! then copies through a `.ansible_tmp*` file), directories the reference would create, and a
//! task `environment`.

use std::collections::BTreeMap;
use std::ffi::CString;
use std::fmt::Write as _;
use std::fs;
use std::io::{self, Read as _};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::Path;

use serde_json::{Map, Value, json};

use super::common::{
    APPEND, AUTOMATIC, Account, ArgSpec, BackupError, Clock, FsError, IMMUTABLE, ModeError,
    Stop as Halt, access, add_path_info, backup_local, bool_param, check, check_names, clock, flag,
    group_account, has_flags, has_xattrs, module_args, native_run, null, owner_account, parent,
    parse_mode, path_param, realpath, selinux_enabled, set_fs_attributes, str_param, umask,
    validate_runs_as_python,
};
use super::setup::run_output;
use super::{Native, NativeRun};
use crate::modules::Context;

pub const NATIVE: Native = Native {
    name: "copy",
    aliases: &[],
    enabled: true,
    run,
};

/// `copy`'s own options, then `add_file_common_args`.
const SPEC: &[ArgSpec] = &[
    null("src"),
    null("_original_basename"),
    null("content"),
    null("dest"),
    flag("backup", false),
    flag("force", true),
    null("validate"),
    null("directory_mode"),
    flag("remote_src", false),
    null("local_follow"),
    null("checksum"),
    flag("follow", false),
    null("mode"),
    null("owner"),
    null("group"),
    null("seuser"),
    null("serole"),
    null("selevel"),
    null("setype"),
    ArgSpec {
        name: "attributes",
        aliases: &["attr"],
        default: || Value::Null,
    },
    flag("unsafe_writes", false),
];

/// Options whose every value other than `null` is outside the native. The action plugin never
/// sends `content`: it stages it as `src`.
const UNSUPPORTED: &[&str] = &[
    "content",
    "directory_mode",
    "attributes",
    "seuser",
    "serole",
    "selevel",
    "setype",
];

/// Why the answer stopped short of a result.
enum Stop {
    /// The reference's `fail_json`, with these keys.
    Fail(Map<String, Value>),
    /// Outside the subset. Only ever returned before anything on the host changed.
    Back(String),
    /// The task's `timeout` ran out, or the controller cancelled it.
    Clock(Halt),
}

impl From<String> for Stop {
    fn from(reason: String) -> Self {
        Stop::Back(reason)
    }
}

impl From<&str> for Stop {
    fn from(reason: &str) -> Self {
        Stop::Back(reason.to_string())
    }
}

impl From<Halt> for Stop {
    fn from(halt: Halt) -> Self {
        match halt {
            Halt::HandBack(reason) => Stop::Back(reason),
            other => Stop::Clock(other),
        }
    }
}

/// What the task asks for, read and checked before anything is touched.
struct Request<'a> {
    src: &'a str,
    dest: &'a str,
    original_basename: Option<&'a str>,
    backup: bool,
    force: bool,
    follow: bool,
    validate: Option<&'a str>,
    checksum: Option<&'a str>,
    owner: Option<Account>,
    group: Option<Account>,
    mode: Option<Value>,
    umask: u32,
}

fn run(args: &Map<String, Value>, context: &Context, cancelled: &dyn Fn() -> bool) -> NativeRun {
    native_run(answer(args, context, clock(context, cancelled)), context)
}

fn answer(
    args: &Map<String, Value>,
    context: &Context,
    clock: Clock,
) -> Result<Map<String, Value>, Halt> {
    if !cfg!(target_os = "linux") {
        return Err("the native answers on Linux only".into());
    }
    if !context.environment.is_empty() {
        return Err("the task sets an environment".into());
    }
    if std::env::var_os("ANSIBLE_UNSAFE_WRITES").is_some() {
        return Err("ANSIBLE_UNSAFE_WRITES is set".into());
    }
    if selinux_enabled() {
        return Err("SELinux is enabled".into());
    }
    // `md5sum` is `null` where Python's hashlib refuses MD5.
    if fs::read_to_string("/proc/sys/crypto/fips_enabled").is_ok_and(|on| on.trim() != "0") {
        return Err("the host is in FIPS mode".into());
    }
    check(clock)?;
    check_names(SPEC, args)?;
    let params = module_args(SPEC, args);
    let request = request(&params, clock)?;
    let mut result = match copy(&request, clock) {
        Ok(result) => result,
        Err(Stop::Fail(mut fail)) => {
            fail.insert("failed".into(), Value::Bool(true));
            fail
        }
        Err(Stop::Back(reason)) => return Err(Halt::HandBack(reason)),
        Err(Stop::Clock(halt)) => return Err(halt),
    };
    add_path_info(&mut result, clock)?;
    result.insert("invocation".into(), json!({"module_args": params}));
    Ok(result)
}

/// The parameters as `AnsibleModule` would validate them. Whatever it would convert, warn about
/// or refuse goes back to the Python module, which says it in its own words.
fn request<'a>(params: &'a Map<String, Value>, clock: Clock) -> Result<Request<'a>, Halt> {
    if let Some(name) = UNSUPPORTED.iter().find(|name| !params[**name].is_null()) {
        return Err(format!("{name} is set").into());
    }
    for name in ["backup", "force", "remote_src", "follow", "unsafe_writes"] {
        bool_param(params, name)?;
    }
    if bool_param(params, "remote_src")? {
        return Err("remote_src is set".into());
    }
    if bool_param(params, "unsafe_writes")? {
        return Err("unsafe_writes is set".into());
    }
    if !matches!(params["local_follow"], Value::Null | Value::Bool(_)) {
        return Err("local_follow is not a boolean".into());
    }
    let text = |name| str_param(params, name);
    let dest = path_param(params, "dest")?;
    // `os.path.dirname` folds repeated slashes, which the messages would show.
    if dest.contains("//") {
        return Err("dest has repeated slashes".into());
    }
    let mode = Some(params["mode"].clone()).filter(|mode| !mode.is_null());
    if let Some(mode) = &mode {
        if *mode == "preserve" {
            return Err("mode is preserve".into());
        }
        if let Err(ModeError::Unsupported(why)) = parse_mode(mode, 0, false) {
            return Err(why.into());
        }
    }
    Ok(Request {
        src: path_param(params, "src")?,
        dest,
        // Read as the reference reads them: an empty one is none at all.
        original_basename: text("_original_basename")?.filter(|base| !base.is_empty()),
        backup: bool_param(params, "backup")?,
        force: bool_param(params, "force")?,
        follow: bool_param(params, "follow")?,
        validate: text("validate")?.filter(|validate| !validate.is_empty()),
        checksum: text("checksum")?.filter(|checksum| !checksum.is_empty()),
        owner: text("owner")?
            .map(|owner| owner_account(owner, clock))
            .transpose()?,
        group: text("group")?
            .map(|group| group_account(group, clock))
            .transpose()?,
        mode,
        umask: umask().map_err(|_| "the umask cannot be read")?,
    })
}

/// `main` for a request inside the subset.
fn copy(request: &Request, clock: Clock) -> Result<Map<String, Value>, Stop> {
    let src = request.src;
    if !(fs::symlink_metadata(src).is_ok_and(|meta| meta.is_file()) && access(src, libc::R_OK)) {
        return Err("src is not a readable regular file".into());
    }
    let (checksum_src, md5sum) = digests(src, clock)?;
    if let Some(expected) = request.checksum
        && expected != checksum_src
    {
        let mut fail =
            message("Copied file does not match the expected checksum. Transfer failed.".into());
        fail.insert("checksum".into(), checksum_src.into());
        fail.insert("expected_checksum".into(), expected.into());
        return Err(Stop::Fail(fail));
    }

    let mut dest = request.dest.to_string();
    if dest.ends_with('/') {
        if let Some(base) = request.original_basename {
            dest = join(&dest, base);
        }
        if fs::metadata(parent(&dest)).is_err() {
            return Err("the destination's directories would be created".into());
        }
    }
    if fs::metadata(&dest).is_ok_and(|meta| meta.is_dir()) {
        let base = request
            .original_basename
            .unwrap_or_else(|| src.rsplit('/').next().unwrap_or(src));
        dest = join(&dest, base);
    }

    let exists = fs::metadata(&dest).is_ok();
    let mut checksum_dest = None;
    if exists {
        if request.follow && is_link(&dest) {
            dest = realpath(&dest)?;
        }
        if !request.force {
            let mut result = message("file already exists".into());
            result.insert("src".into(), src.into());
            result.insert("dest".into(), dest.into());
            result.insert("changed".into(), Value::Bool(false));
            return Ok(result);
        }
        // The reference goes on without a sum and fails, or replaces a device, on its way.
        if !(access(&dest, libc::R_OK) && fs::metadata(&dest).is_ok_and(|meta| meta.is_file())) {
            return Err("dest is not a readable regular file".into());
        }
        checksum_dest = Some(sha1_of(&dest, clock)?);
    } else {
        let dir = parent(&dest);
        if let Err(err) = fs::metadata(dir) {
            let msg = if err.raw_os_error() == Some(libc::EACCES) {
                format!("Destination directory {dir} is not accessible")
            } else {
                format!("Destination directory {dir} does not exist")
            };
            return Err(Stop::Fail(message(msg)));
        }
    }
    let dir = parent(&dest);
    if !access(dir, libc::W_OK) {
        return Err(Stop::Fail(message(format!(
            "Destination {dir} not writable"
        ))));
    }

    let link = is_link(&dest);
    let mut changed = false;
    let mut backup_file = None;
    if checksum_dest.as_ref() != Some(&checksum_src) || link {
        preflight(request, &dest, exists, link)?;
        check(clock)?;
        if request.backup && exists {
            backup_file = Some(match backup_local(&dest, clock) {
                Ok(name) => name,
                Err(BackupError::Exists(name)) => return Err(format!("{name} exists").into()),
                Err(BackupError::Failed(msg)) => return Err(Stop::Fail(message(msg))),
                Err(BackupError::Stopped(halt)) => return Err(Stop::Clock(halt)),
            });
        }
        let failed = |_| Stop::Fail(message(format!("Failed to copy '{src}' to '{dest}'.")));
        // A link is replaced by a file of its own, not written through.
        if link {
            fs::remove_file(&dest).map_err(failed)?;
            fs::File::create(&dest).map_err(failed)?;
        }
        if let Some(validate) = request.validate {
            validate_src(request, validate, clock)?;
        }
        atomic_move(src, &dest, request.umask).map_err(|msg| Stop::Fail(message(msg)))?;
        changed = true;
    }

    match set_fs_attributes(
        &dest,
        request.owner.as_ref(),
        request.group.as_ref(),
        request.mode.as_ref(),
    ) {
        Ok(attributes) => changed |= attributes,
        Err(FsError::Failed(fail)) => return Err(Stop::Fail(fail)),
        Err(FsError::Raised(error, touched)) if changed || touched => {
            return Err(Stop::Fail(message(error)));
        }
        Err(FsError::Raised(error, _)) => return Err(Stop::Back(error)),
    }
    let mut result = Map::new();
    result.insert("dest".into(), dest.into());
    result.insert("src".into(), src.into());
    result.insert("md5sum".into(), md5sum.into());
    result.insert("checksum".into(), checksum_src.into());
    result.insert("changed".into(), Value::Bool(changed));
    if let Some(backup_file) = backup_file {
        result.insert("backup_file".into(), backup_file.into());
    }
    Ok(result)
}

/// What would send the move down the reference's workarounds, or copy what the native does not,
/// checked before its first change. `exists` is whether `dest` resolves to a file, `link`
/// whether it is a link the native replaces.
fn preflight(request: &Request, dest: &str, exists: bool, link: bool) -> Result<(), String> {
    let dir = parent(dest);
    let dir_meta = fs::metadata(dir).map_err(|err| format!("{dir}: {err}"))?;
    if !dir_meta.is_dir() {
        return Err(format!("{dir} is not a directory"));
    }
    // A rename needs to search the directory too; `access` above only asked for writing.
    if !access(dir, libc::W_OK | libc::X_OK) {
        return Err(format!("{dir} is not writable"));
    }
    // `rename` crosses neither file systems nor mounts, and cannot replace a mount point.
    let mount = mount_id(dir, true).ok_or("the mount of the destination cannot be read")?;
    let at_dir = (dir_meta.dev(), Some(mount));
    let src_meta = fs::symlink_metadata(request.src).map_err(|err| format!("src: {err}"))?;
    if crosses((src_meta.dev(), mount_id(request.src, false)), at_dir) {
        return Err(
            "the staged source is on another file system or mount than the destination".into(),
        );
    }
    let here = fs::symlink_metadata(dest).ok();
    if let Some(meta) = &here
        && crosses((meta.dev(), mount_id(dest, false)), at_dir)
    {
        return Err("the destination is a mount point or a subvolume of its own".into());
    }
    // A sticky directory keeps others' files from being replaced or unlinked.
    // SAFETY: a plain getter.
    let euid = unsafe { libc::geteuid() };
    if dir_meta.mode() & 0o1000 != 0
        && euid != 0
        && dir_meta.uid() != euid
        && here.is_some_and(|meta| meta.uid() != euid)
    {
        return Err("the sticky directory keeps the destination from being replaced".into());
    }
    if has_flags(dir, APPEND) {
        return Err("the directory is append-only".into());
    }
    if exists && !link && has_flags(dest, IMMUTABLE | APPEND) {
        return Err("the destination is immutable or append-only".into());
    }
    // `shutil.copystat` gives the source the destination's extended attributes, and
    // `preserved_copy` gives them to the backup, through a link too.
    if exists && (!link || request.backup) && has_xattrs(dest) {
        return Err("the destination has extended attributes".into());
    }
    // `preserved_copy` gives the backup the flags `lsattr` shows, through a link too.
    if exists && request.backup && has_flags(dest, !AUTOMATIC) {
        return Err("the destination has inode flags the backup would carry".into());
    }
    // The empty file standing for the link takes the directory's default ACL, which
    // `copystat` then gives the source.
    if link && has_xattrs(dir) {
        return Err("the directory has extended attributes".into());
    }
    match request.validate.filter(|validate| validate.contains("%s")) {
        Some(validate) => validate_runs_as_python(validate, request.src, "src"),
        None => Ok(()),
    }
}

/// Whether a rename between two places, each given by its device and its mount (`None` where the
/// kernel does not say), would fail with EXDEV or EBUSY. The two can differ apart: a btrfs
/// subvolume has a device of its own on its parent's mount, and a bind mount shows its file
/// system's device on a mount of its own.
fn crosses(a: (u64, Option<u64>), b: (u64, Option<u64>)) -> bool {
    a.0 != b.0 || a.1.is_none() || a.1 != b.1
}

/// The source given the task's mode, then owner and group, then `validate` run on it, as the
/// reference does before the move. Anything that stops here comes after the backup: a failure,
/// never a hand-back.
fn validate_src(request: &Request, validate: &str, clock: Clock) -> Result<(), Stop> {
    let src = request.src;
    let attributes = set_fs_attributes(src, None, None, request.mode.as_ref())
        .and_then(|_| set_fs_attributes(src, request.owner.as_ref(), request.group.as_ref(), None));
    match attributes {
        Ok(_) => {}
        Err(FsError::Failed(fail)) => return Err(Stop::Fail(fail)),
        Err(FsError::Raised(error, _)) => return Err(Stop::Fail(message(error))),
    }
    if !validate.contains("%s") {
        return Err(Stop::Fail(message(format!(
            "validate must contain %s: {validate}"
        ))));
    }
    let argv = shlex::split(&validate.replace("%s", src)).unwrap_or_default();
    let (program, args) = argv
        .split_first()
        .ok_or_else(|| Stop::Fail(message(format!("validate names no program: {validate}"))))?;
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let ran =
        run_output(&BTreeMap::new(), clock, Path::new(program), &args).map_err(
            |halt| match halt {
                Halt::HandBack(reason) => Stop::Fail(message(reason)),
                other => Stop::Clock(other),
            },
        )?;
    match ran {
        Ok((0, _, _)) => Ok(()),
        Ok((rc, out, err)) => {
            let mut fail = message("failed to validate".into());
            fail.insert("exit_status".into(), rc.into());
            fail.insert("stdout".into(), out.into());
            fail.insert("stderr".into(), err.into());
            Err(Stop::Fail(fail))
        }
        // Checked up front; only a program removed since then gets here. `run_command`'s
        // answer to the `OSError`.
        Err(errno) => {
            let mut fail = message("Error executing command.".into());
            fail.insert("rc".into(), errno.into());
            fail.insert("stdout".into(), "".into());
            fail.insert("stderr".into(), "".into());
            fail.insert("cmd".into(), argv.join(" ").into());
            Err(Stop::Fail(fail))
        }
    }
}

/// `atomic_move(src, dest)` with `keep_dest_attrs`, for a move `preflight` let through: the
/// source given the replaced file's owner, its mode and the time now, then renamed over it. A
/// refused `chown` skips the mode and the time as well, all three sitting in one `try`. A file
/// created gets `0666` less the umask, and the running account.
///
/// The errors are the reference's exception texts. It ends on them with a traceback where this
/// returns them as the failure's message; `preflight` leaves only a race to reach them.
fn atomic_move(src: &str, dest: &str, umask: u32) -> Result<(), String> {
    let failed = |_: io::Error| format!("Failed to copy '{src}' to '{dest}'.");
    if let Ok(stat) = fs::metadata(dest) {
        match std::os::unix::fs::chown(src, Some(stat.uid()), Some(stat.gid())) {
            Ok(()) => {
                fs::set_permissions(src, fs::Permissions::from_mode(stat.mode() & 0o7777))
                    .map_err(failed)?;
                touch(src).map_err(failed)?;
            }
            Err(err) if err.raw_os_error() == Some(libc::EPERM) => {}
            Err(err) => return Err(failed(err)),
        }
    }
    let creating = fs::metadata(dest).is_err();
    fs::rename(src, dest).map_err(|_| format!("Could not replace '{dest}' with '{src}'."))?;
    if creating {
        fs::set_permissions(dest, fs::Permissions::from_mode(0o666 & !umask)).map_err(failed)?;
        let dir = fs::metadata(parent(dest)).map_err(failed)?;
        // SAFETY: plain getters.
        let (euid, egid) = unsafe { (libc::geteuid(), libc::getegid()) };
        let gid = if dir.mode() & 0o2000 != 0 {
            dir.gid()
        } else {
            egid
        };
        let _ = std::os::unix::fs::chown(dest, Some(euid), Some(gid));
    }
    Ok(())
}

/// `os.utime(path, times=(time.time(), time.time()))`.
fn touch(path: &str) -> io::Result<()> {
    let path = CString::new(path).map_err(io::Error::other)?;
    // SAFETY: a null `times` sets both to now.
    let rc = unsafe { libc::utimensat(libc::AT_FDCWD, path.as_ptr(), std::ptr::null(), 0) };
    if rc == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

fn is_link(path: &str) -> bool {
    fs::symlink_metadata(path).is_ok_and(|meta| meta.file_type().is_symlink())
}

/// `os.path.join(dir, name)`.
fn join(dir: &str, name: &str) -> String {
    if name.starts_with('/') {
        name.to_string()
    } else if dir.ends_with('/') {
        format!("{dir}{name}")
    } else {
        format!("{dir}/{name}")
    }
}

fn message(msg: String) -> Map<String, Value> {
    let mut map = Map::new();
    map.insert("msg".into(), msg.into());
    map
}

/// `module.sha1(path)` and `module.md5(path)`, read once.
///
/// The deadline and the cancel are looked at every 16 blocks (1 MiB), as the Python module would
/// be killed at the task's `timeout` in the middle of a large file.
fn digests(path: &str, clock: Clock) -> Result<(String, String), Stop> {
    let mut md5 = Md5::new();
    let sha1 = read_blocks(path, clock, |block| md5.update(block))?;
    Ok((sha1, md5.hex()))
}

fn sha1_of(path: &str, clock: Clock) -> Result<String, Stop> {
    read_blocks(path, clock, |_| {})
}

/// The SHA-1 of the file at `path`, each block also given to `each`.
fn read_blocks(path: &str, clock: Clock, mut each: impl FnMut(&[u8])) -> Result<String, Stop> {
    let mut file = fs::File::open(path).map_err(|err| format!("reading {path}: {err}"))?;
    let mut sha1 = sha1_smol::Sha1::new();
    let mut block = vec![0; 64 * 1024];
    for round in 0_u64.. {
        if round % 16 == 0 {
            check(clock)?;
        }
        match file.read(&mut block) {
            Ok(0) => break,
            Ok(n) => {
                sha1.update(&block[..n]);
                each(&block[..n]);
            }
            Err(err) => return Err(format!("reading {path}: {err}").into()),
        }
    }
    Ok(sha1.digest().to_string())
}

/// MD5 (RFC 1321), for `md5sum`: no crate of the workspace has it.
struct Md5 {
    state: [u32; 4],
    block: [u8; 64],
    filled: usize,
    len: u64,
}

/// `floor(abs(sin(i + 1)) * 2^32)`.
const MD5_K: [u32; 64] = [
    0xd76a_a478,
    0xe8c7_b756,
    0x2420_70db,
    0xc1bd_ceee,
    0xf57c_0faf,
    0x4787_c62a,
    0xa830_4613,
    0xfd46_9501,
    0x6980_98d8,
    0x8b44_f7af,
    0xffff_5bb1,
    0x895c_d7be,
    0x6b90_1122,
    0xfd98_7193,
    0xa679_438e,
    0x49b4_0821,
    0xf61e_2562,
    0xc040_b340,
    0x265e_5a51,
    0xe9b6_c7aa,
    0xd62f_105d,
    0x0244_1453,
    0xd8a1_e681,
    0xe7d3_fbc8,
    0x21e1_cde6,
    0xc337_07d6,
    0xf4d5_0d87,
    0x455a_14ed,
    0xa9e3_e905,
    0xfcef_a3f8,
    0x676f_02d9,
    0x8d2a_4c8a,
    0xfffa_3942,
    0x8771_f681,
    0x6d9d_6122,
    0xfde5_380c,
    0xa4be_ea44,
    0x4bde_cfa9,
    0xf6bb_4b60,
    0xbebf_bc70,
    0x289b_7ec6,
    0xeaa1_27fa,
    0xd4ef_3085,
    0x0488_1d05,
    0xd9d4_d039,
    0xe6db_99e5,
    0x1fa2_7cf8,
    0xc4ac_5665,
    0xf429_2244,
    0x432a_ff97,
    0xab94_23a7,
    0xfc93_a039,
    0x655b_59c3,
    0x8f0c_cc92,
    0xffef_f47d,
    0x8584_5dd1,
    0x6fa8_7e4f,
    0xfe2c_e6e0,
    0xa301_4314,
    0x4e08_11a1,
    0xf753_7e82,
    0xbd3a_f235,
    0x2ad7_d2bb,
    0xeb86_d391,
];

/// The left rotations of each round, four per round.
const MD5_S: [[u32; 4]; 4] = [
    [7, 12, 17, 22],
    [5, 9, 14, 20],
    [4, 11, 16, 23],
    [6, 10, 15, 21],
];

impl Md5 {
    fn new() -> Self {
        Md5 {
            state: [0x6745_2301, 0xefcd_ab89, 0x98ba_dcfe, 0x1032_5476],
            block: [0; 64],
            filled: 0,
            len: 0,
        }
    }

    fn update(&mut self, mut data: &[u8]) {
        self.len = self.len.wrapping_add(data.len() as u64);
        while !data.is_empty() {
            let take = (64 - self.filled).min(data.len());
            self.block[self.filled..self.filled + take].copy_from_slice(&data[..take]);
            self.filled += take;
            data = &data[take..];
            if self.filled == 64 {
                self.compress();
                self.filled = 0;
            }
        }
    }

    fn compress(&mut self) {
        let mut m = [0_u32; 16];
        for (word, bytes) in m.iter_mut().zip(self.block.as_chunks::<4>().0) {
            *word = u32::from_le_bytes(*bytes);
        }
        let [mut a, mut b, mut c, mut d] = self.state;
        for i in 0..64 {
            let (f, g) = match i / 16 {
                0 => ((b & c) | (!b & d), i),
                1 => ((d & b) | (!d & c), (5 * i + 1) % 16),
                2 => (b ^ c ^ d, (3 * i + 5) % 16),
                _ => (c ^ (b | !d), (7 * i) % 16),
            };
            let turned = a
                .wrapping_add(f)
                .wrapping_add(MD5_K[i])
                .wrapping_add(m[g])
                .rotate_left(MD5_S[i / 16][i % 4]);
            a = d;
            d = c;
            c = b;
            b = b.wrapping_add(turned);
        }
        for (word, add) in self.state.iter_mut().zip([a, b, c, d]) {
            *word = word.wrapping_add(add);
        }
    }

    fn hex(mut self) -> String {
        let bits = self.len.wrapping_mul(8);
        self.update(&[0x80]);
        while self.filled != 56 {
            self.update(&[0]);
        }
        self.update(&bits.to_le_bytes());
        let mut out = String::with_capacity(32);
        for byte in self.state.iter().flat_map(|word| word.to_le_bytes()) {
            let _ = write!(out, "{byte:02x}");
        }
        out
    }
}

/// `STATX_MNT_ID`, which the `libc` crate declares for glibc only.
#[cfg(target_os = "linux")]
const STATX_MNT_ID: libc::c_uint = 0x1000;

/// The id of the mount `path` is on (`statx`, Linux 5.8 and later), or `None` when the kernel
/// does not say.
#[cfg(target_os = "linux")]
fn mount_id(path: &str, follow: bool) -> Option<u64> {
    /// A `struct statx`, as bytes: `stx_mask` first, `stx_mnt_id` at byte 144.
    #[repr(C, align(8))]
    struct Statx([u8; 256]);
    let path = CString::new(path).ok()?;
    let mut buf = Statx([0; 256]);
    let flags = if follow { 0 } else { libc::AT_SYMLINK_NOFOLLOW };
    // SAFETY: `buf` is larger than the `struct statx` the kernel writes.
    let rc = unsafe {
        libc::syscall(
            libc::SYS_statx,
            libc::AT_FDCWD,
            path.as_ptr(),
            flags,
            STATX_MNT_ID,
            buf.0.as_mut_ptr(),
        )
    };
    let mask = u32::from_ne_bytes(buf.0[0..4].try_into().ok()?);
    if rc != 0 || mask & STATX_MNT_ID == 0 {
        return None;
    }
    Some(u64::from_ne_bytes(buf.0[144..152].try_into().ok()?))
}

/// Elsewhere the native hands every task back before it gets here.
#[cfg(not(target_os = "linux"))]
fn mount_id(_: &str, _: bool) -> Option<u64> {
    None
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::{Duration, SystemTime};

    use regex::Regex;
    use serde_json::{Value, json};
    use volant_protocol::TaskResult;

    use super::super::common::golden::{Scratch, differences, within};
    use super::*;

    const GOLDEN: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../volant/tests/golden/native");

    /// The recordings were taken under a `022` umask, which sets the mode of a file created.
    /// Nextest runs each test in a process of its own.
    fn scratch(name: &str) -> Scratch {
        unsafe { libc::umask(0o022) };
        let scratch = Scratch::new(name);
        fs::create_dir(scratch.path("s")).unwrap();
        scratch
    }

    /// `content` staged as the agent stages a task's file, in a file of its own, `0600`, and
    /// dated long ago, so that only `atomic_move`'s `utime` can give a replaced file the time now.
    fn stage(scratch: &Scratch, content: &str) -> String {
        let src = scratch.path("s/src");
        write(&src, content, 0o600);
        set_old_time(&src);
        src
    }

    fn set_old_time(path: &str) {
        fs::File::options()
            .write(true)
            .open(path)
            .unwrap()
            .set_modified(SystemTime::UNIX_EPOCH + Duration::from_secs(978_307_200))
            .unwrap();
    }

    fn write(path: &str, content: &str, mode: u32) {
        fs::write(path, content).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
    }

    fn ask(args: &Value) -> NativeRun {
        ask_in(args, &Context::default())
    }

    fn ask_in(args: &Value, context: &Context) -> NativeRun {
        run(args.as_object().unwrap(), context, &|| false)
    }

    fn sha1(content: &str) -> String {
        let mut sha1 = sha1_smol::Sha1::new();
        sha1.update(content.as_bytes());
        sha1.digest().to_string()
    }

    /// What the golden test reads back after a case: `{exists, mode, type, content}`, and the
    /// backup's content when there is one.
    fn after(path: &str, backup: Option<&str>) -> Value {
        let Ok(meta) = fs::symlink_metadata(path) else {
            return json!({"exists": false});
        };
        let kind = if meta.is_dir() { "directory" } else { "file" };
        let mode = format!("{:04o}", meta.mode() & 0o7777);
        let mut after = json!({"exists": true, "mode": mode, "type": kind});
        if kind == "file" {
            after["content"] = fs::read_to_string(path).unwrap().into();
        }
        if let Some(backup) = backup {
            after["backup_content"] = fs::read_to_string(backup).unwrap().into();
        }
        after
    }

    /// Every path under the scratch directory, its mode, and what a file holds.
    fn snapshot(scratch: &Scratch) -> Vec<(String, u32, Vec<u8>)> {
        let mut found = Vec::new();
        let mut dirs = vec![scratch.0.clone()];
        while let Some(dir) = dirs.pop() {
            for entry in fs::read_dir(&dir).unwrap() {
                let path = entry.unwrap().path().to_string_lossy().into_owned();
                let meta = fs::symlink_metadata(&path).unwrap();
                if meta.is_dir() {
                    dirs.push(path.clone());
                }
                let content = if meta.is_file() {
                    fs::read(&path).unwrap()
                } else {
                    Vec::new()
                };
                found.push((path, meta.mode(), content));
            }
        }
        found.sort();
        found
    }

    /// Checks `ours` against `want` key by key, a backup's name against the reference's pattern
    /// first, and pushes what differs to `found`. Returns the backup's name.
    fn compare(
        case: &str,
        want: &str,
        mut ours: Map<String, Value>,
        scratch: &Scratch,
        volatile: &[&str],
        found: &mut Vec<String>,
    ) -> Option<String> {
        let recorded: Value = serde_json::from_str(want).unwrap();
        let backup = ours
            .get("backup_file")
            .and_then(Value::as_str)
            .map(str::to_string);
        if let Some(name) = &backup {
            let masked = name.replace(&scratch.0, "<golden-tmp>");
            let dest = recorded["dest"].as_str().unwrap_or_default();
            let pattern = format!(
                r"^{}\.\d+\.\d{{4}}-\d{{2}}-\d{{2}}@\d{{2}}:\d{{2}}:\d{{2}}~$",
                regex::escape(dest)
            );
            if !Regex::new(&pattern).unwrap().is_match(&masked) {
                found.push(format!("{case}: backup {masked} is not named {pattern}"));
            }
            ours.insert("backup_file".into(), recorded["backup_file"].clone());
        }
        found.extend(
            differences(want, ours, scratch, volatile)
                .into_iter()
                .map(|d| format!("{case}: {d}")),
        );
        backup
    }

    /// Every recorded `copy-module-*` case, played in the golden play's order on the same files,
    /// with the arguments the action plugin sends for `content`: the text staged as `src`, its
    /// sum as `checksum`, `_original_basename` and `follow`. What the plugin and the controller
    /// add to the module's answer (`diff`, the sum of a failure, `stdout_lines`) is added here
    /// too. `remote_src` must hand back with nothing changed.
    ///
    /// What would make this red: `validate` run after the move (`copy-module-validate-fail`
    /// leaves no file), the source copied rather than moved (it survives, or the file is another
    /// inode), a backup named otherwise or holding other content, a message worded otherwise, or
    /// `remote_src` answered.
    #[test]
    fn copy_answers_every_recorded_case_like_the_reference() {
        let scratch = scratch("copy");
        scratch.fixture();
        let index: Value =
            serde_json::from_str(&fs::read_to_string(format!("{GOLDEN}/index.json")).unwrap())
                .unwrap();
        let mut found = Vec::new();
        for case in [
            "copy-module-created",
            "copy-module-modified-backup",
            "copy-module-validate-fail",
            "copy-module-no-dir",
            "copy-module-remote-src",
            "copy-module-checksum",
            "copy-module-dest-link",
        ] {
            let entry = &index[case];
            let recording = fs::read_to_string(format!("{GOLDEN}/{case}.json")).unwrap();
            let want: Value = serde_json::from_str(&recording).unwrap();
            let mut args: Value = serde_json::from_str(
                &entry["args"]
                    .to_string()
                    .replace("<golden-tmp>", &scratch.0),
            )
            .unwrap();
            let dest = args["dest"].as_str().unwrap().to_string();
            let mut staged = None;
            if let Some(content) = args.as_object_mut().unwrap().remove("content") {
                let content = content.as_str().unwrap().to_string();
                let src = stage(&scratch, &content);
                args["src"] = src.clone().into();
                // The plugin's sum only where the task gave none.
                if args.get("checksum").is_none() {
                    args["checksum"] = sha1(&content).into();
                }
                args["_original_basename"] = ".staged".into();
                args["follow"] = false.into();
                staged = Some((src.clone(), fs::metadata(&src).unwrap().ino(), content));
            }
            let before = snapshot(&scratch);
            let answer = ask(&args);
            if entry["expect"] == "fallback" {
                if !matches!(answer, NativeRun::Fallback(_)) {
                    found.push(format!("{case}: answered outside the subset"));
                }
                if snapshot(&scratch) != before {
                    found.push(format!("{case}: changed something before handing back"));
                }
                continue;
            }
            let NativeRun::Done(result) = answer else {
                found.push(format!("{case}: handed back or cancelled"));
                continue;
            };
            let (src, ino, content) = staged.unwrap();
            let mut ours = result.0;
            ours.entry("diff").or_insert_with(|| json!([]));
            ours.entry("checksum")
                .or_insert_with(|| sha1(&content).into());
            for key in ["stdout", "stderr"] {
                if let Some(text) = ours.get(key).and_then(Value::as_str) {
                    let lines: Vec<Value> = text.lines().map(Value::from).collect();
                    ours.insert(format!("{key}_lines"), lines.into());
                }
            }
            if ours.get("src") == Some(&json!(src)) {
                ours.insert("src".into(), "<golden-staged-path>".into());
            }
            ours["invocation"]["module_args"]["src"] = "<golden-staged-path>".into();
            if want["changed"] == true
                && (Path::new(&src).exists() || fs::metadata(&dest).unwrap().ino() != ino)
            {
                found.push(format!("{case}: the staged source was copied, not moved"));
            }
            let volatile: Vec<&str> = entry["volatile"]
                .as_array()
                .unwrap()
                .iter()
                .filter_map(Value::as_str)
                .collect();
            let backup = compare(case, &recording, ours, &scratch, &volatile, &mut found);
            let left = after(&dest, backup.as_deref());
            if left != want["_after"] {
                found.push(format!(
                    "{case}: left {left}, the reference {}",
                    want["_after"]
                ));
            }
        }
        assert!(found.is_empty(), "{found:#?}");
    }

    /// The branches the golden play does not reach, each against what ansible-core 2.19.12's
    /// module answered with the same files and arguments (measured by running
    /// `python -m ansible.modules.copy` on an arguments file, no action plugin): a link as `dest`
    /// without `follow` (its target kept, which the recorded `copy-module-dest-link` does not
    /// read) and with it (with a backup), a dangling link,
    /// the same content with another mode, `force: false`, a directory as `dest` with and
    /// without its slash, an unknown owner, `validate` without `%s`, a file replaced, and an
    /// invalid mode. A failure gets the `changed: false` ansible-core's task executor adds, as
    /// the dispatcher adds it; `invocation` is held to the reference by the recorded cases.
    ///
    /// What would make this red: a link written through instead of replaced, the backup of a
    /// followed link named after the link, the same content moved anyway (the staged source
    /// gone), `atomic_move` without its `utime` (the replaced file keeps its old time) or
    /// without the replaced file's mode, or a failure coming before the move where the reference
    /// fails after it (`owner-unknown`, `mode-invalid` leave the file).
    #[test]
    fn copy_matches_the_reference_on_every_other_branch() {
        let scratch = scratch("copy-branches");
        let t = scratch.0.clone();
        let mut found = Vec::new();
        let mut case = |name: &str, content: &str, args: Value, want: &str| {
            let src = stage(&scratch, content);
            let mut args: Value =
                serde_json::from_str(&args.to_string().replace("<t>", &t)).unwrap();
            args["src"] = src.into();
            let NativeRun::Done(result) = ask(&args) else {
                found.push(format!("{name}: handed back or cancelled"));
                return None;
            };
            let mut ours = result.0;
            ours.remove("invocation");
            compare(name, want, ours, &scratch, &[], &mut found)
        };
        let path = |name: &str| format!("{t}/{name}");

        write(&path("tgt"), "target\n", 0o640);
        std::os::unix::fs::symlink("tgt", path("lnk")).unwrap();
        case(
            "dest-link",
            "one\n",
            json!({"dest": "<t>/lnk", "checksum": "c7059bb19433cc3cabaa6236c83d56668a843dd2", "_original_basename": "x"}),
            r#"{"dest": "<golden-tmp>/lnk", "src": "<golden-tmp>/s/src", "md5sum": "5bbf5a52328e7439ae6e719dfe712200", "checksum": "c7059bb19433cc3cabaa6236c83d56668a843dd2", "changed": true, "uid": "<uid>", "gid": "<gid>", "owner": "<user>", "group": "<group>", "mode": "0644", "state": "file", "size": 4}"#,
        );
        let link_left = (after(&path("lnk"), None), after(&path("tgt"), None));

        std::os::unix::fs::symlink("tgt", path("lnk2")).unwrap();
        let backup = case(
            "dest-link-follow",
            "two\n",
            json!({"dest": "<t>/lnk2", "follow": true, "backup": true}),
            r#"{"dest": "<golden-tmp>/tgt", "src": "<golden-tmp>/s/src", "md5sum": "c193497a1a06b2c72230e6146ff47080", "checksum": "7bbef45b3bc70855010e02460717643125c3beca", "changed": true, "backup_file": "<golden-tmp>/tgt.347163.2026-09-27@14:25:29~", "uid": "<uid>", "gid": "<gid>", "owner": "<user>", "group": "<group>", "mode": "0640", "state": "file", "size": 4}"#,
        );
        let follow_left = (
            is_link(&path("lnk2")),
            after(&path("tgt"), backup.as_deref()),
        );

        std::os::unix::fs::symlink("nowhere", path("dangling")).unwrap();
        case(
            "dest-dangling",
            "d\n",
            json!({"dest": "<t>/dangling"}),
            r#"{"dest": "<golden-tmp>/dangling", "src": "<golden-tmp>/s/src", "md5sum": "e29311f6f1bf1af907f9ef9f44b8328b", "checksum": "e983f374794de9c64e3d1c1de1d490c0756eeeff", "changed": true, "uid": "<uid>", "gid": "<gid>", "owner": "<user>", "group": "<group>", "mode": "0644", "state": "file", "size": 2}"#,
        );
        let dangling_left = after(&path("dangling"), None);

        write(&path("same.txt"), "same\n", 0o644);
        case(
            "same-mode",
            "same\n",
            json!({"dest": "<t>/same.txt", "mode": "0600"}),
            r#"{"dest": "<golden-tmp>/same.txt", "src": "<golden-tmp>/s/src", "md5sum": "847676261680bff61c72961c8198abc0", "checksum": "2c985b161217a952b7a410fd91495cebc349f520", "changed": true, "uid": "<uid>", "gid": "<gid>", "owner": "<user>", "group": "<group>", "mode": "0600", "state": "file", "size": 5}"#,
        );
        let same_kept_source = Path::new(&path("s/src")).exists();

        case(
            "force-false",
            "f\n",
            json!({"dest": "<t>/same.txt", "force": false}),
            r#"{"msg": "file already exists", "src": "<golden-tmp>/s/src", "dest": "<golden-tmp>/same.txt", "changed": false, "uid": "<uid>", "gid": "<gid>", "owner": "<user>", "group": "<group>", "mode": "0600", "state": "file", "size": 5}"#,
        );

        fs::create_dir(path("d")).unwrap();
        case(
            "dest-dir",
            "in dir\n",
            json!({"dest": "<t>/d", "_original_basename": "b.txt"}),
            r#"{"dest": "<golden-tmp>/d/b.txt", "src": "<golden-tmp>/s/src", "md5sum": "739df3538be9245dcb11f80816b3b4bb", "checksum": "ff11801174e1238a93abfc875b818b855464be4e", "changed": true, "uid": "<uid>", "gid": "<gid>", "owner": "<user>", "group": "<group>", "mode": "0644", "state": "file", "size": 7}"#,
        );
        case(
            "dest-dir-slash",
            "in dir2\n",
            json!({"dest": "<t>/d/", "_original_basename": "b2.txt"}),
            r#"{"dest": "<golden-tmp>/d/b2.txt", "src": "<golden-tmp>/s/src", "md5sum": "71b9d43c97402befed6f21701bf78e38", "checksum": "6cba982828e45e03128be9d634ca392be0a9aa9c", "changed": true, "uid": "<uid>", "gid": "<gid>", "owner": "<user>", "group": "<group>", "mode": "0644", "state": "file", "size": 8}"#,
        );

        case(
            "owner-unknown",
            "o\n",
            json!({"dest": "<t>/o.txt", "owner": "volant-no-such-user"}),
            r#"{"changed": false, "path": "<golden-tmp>/o.txt", "failed": true, "msg": "chown failed: failed to look up user volant-no-such-user", "uid": "<uid>", "gid": "<gid>", "owner": "<user>", "group": "<group>", "mode": "0644", "state": "file", "size": 2}"#,
        );
        let owner_left = after(&path("o.txt"), None);

        case(
            "validate-no-pct",
            "v\n",
            json!({"dest": "<t>/v.txt", "validate": "true"}),
            r#"{"changed": false, "failed": true, "msg": "validate must contain %s: true"}"#,
        );
        let validate_left = after(&path("v.txt"), None);

        write(&path("e.txt"), "old\n", 0o604);
        set_old_time(&path("e.txt"));
        let started = SystemTime::now() - Duration::from_secs(1);
        case(
            "existing",
            "e\n",
            json!({"dest": "<t>/e.txt"}),
            r#"{"dest": "<golden-tmp>/e.txt", "src": "<golden-tmp>/s/src", "md5sum": "9ffbf43126e33be52cd2bf7e01d627f9", "checksum": "094e3afb2fe8dfe82f63731cdcd3b999f4856cff", "changed": true, "uid": "<uid>", "gid": "<gid>", "owner": "<user>", "group": "<group>", "mode": "0604", "state": "file", "size": 2}"#,
        );
        let replaced_time = fs::metadata(path("e.txt")).unwrap().modified().unwrap();

        case(
            "mode-invalid",
            "m\n",
            json!({"dest": "<t>/m.txt", "mode": "u=zz"}),
            r#"{"changed": false, "path": "<golden-tmp>/m.txt", "details": "bad symbolic permission for mode: u=zz", "failed": true, "msg": "mode must be in octal or symbolic form", "uid": "<uid>", "gid": "<gid>", "owner": "<user>", "group": "<group>", "mode": "0644", "state": "file", "size": 2}"#,
        );
        let mode_left = after(&path("m.txt"), None);

        fs::create_dir(path("ro")).unwrap();
        fs::set_permissions(path("ro"), fs::Permissions::from_mode(0o555)).unwrap();
        case(
            "not-writable",
            "x\n",
            json!({"dest": "<t>/ro/x.txt"}),
            r#"{"changed": false, "failed": true, "msg": "Destination <golden-tmp>/ro not writable"}"#,
        );
        fs::create_dir_all(path("nox/in")).unwrap();
        fs::set_permissions(path("nox"), fs::Permissions::from_mode(0o600)).unwrap();
        case(
            "not-accessible",
            "x\n",
            json!({"dest": "<t>/nox/in/x.txt"}),
            r#"{"changed": false, "failed": true, "msg": "Destination directory <golden-tmp>/nox/in is not accessible"}"#,
        );
        fs::set_permissions(path("nox"), fs::Permissions::from_mode(0o755)).unwrap();

        let file = |content: &str, mode: &str| json!({"exists": true, "mode": mode, "type": "file", "content": content});
        let followed = json!({
            "exists": true, "mode": "0640", "type": "file", "content": "two\n",
            "backup_content": "target\n",
        });
        for (name, left, want) in [
            ("dest-link", link_left.0, file("one\n", "0644")),
            ("dest-link target", link_left.1, file("target\n", "0640")),
            ("dest-link-follow link", json!(follow_left.0), json!(true)),
            ("dest-link-follow", follow_left.1, followed),
            ("dest-dangling", dangling_left, file("d\n", "0644")),
            ("same-mode source", json!(same_kept_source), json!(true)),
            ("owner-unknown", owner_left, file("o\n", "0644")),
            ("validate-no-pct", validate_left, json!({"exists": false})),
            ("mode-invalid", mode_left, file("m\n", "0644")),
        ] {
            if left != want {
                found.push(format!("{name}: left {left}, the reference {want}"));
            }
        }
        if replaced_time < started {
            found.push("existing: the replaced file kept its old time".into());
        }
        assert!(found.is_empty(), "{found:#?}");
    }

    /// Every case outside the subset hands back with every file as it was, the staged source
    /// included: options the native leaves to Python, arguments `AnsibleModule` would convert or
    /// refuse, paths the reference reads another way, directories it would create, a
    /// destination that is a directory, the parent a file, a task `environment`; and, on the
    /// fixture file and on a link to it with `backup`, a `validate` Python would format, expand
    /// or split otherwise or cannot run, inode flags or extended attributes the backup would
    /// carry. The file systems the tests run on take user xattrs and the owner's `nodump` flag;
    /// one that does not fails the test saying so.
    ///
    /// What would make this red: any of these answered, or a hand-back after the backup or the
    /// link's replacement (the snapshot then holds a backup file, or a file where the link was).
    #[test]
    fn copy_hands_back_before_changing_anything() {
        let scratch = scratch("copy-back");
        let t = scratch.0.clone();
        let file = scratch.fixture();
        fs::create_dir_all(scratch.path("d/sub.txt")).unwrap();
        // Searchable, so that only its kind tells it from a directory.
        let exe = scratch.path("exe");
        write(&exe, "", 0o755);
        let src = stage(&scratch, "new\n");
        let mut found = Vec::new();
        let mut hand_back = |name: &str, extra: Value, context: &Context| {
            let mut args = json!({"src": src, "dest": format!("{t}/n.txt"), "backup": true});
            for (key, value) in extra.as_object().unwrap() {
                args[key] = value.clone();
            }
            let before = snapshot(&scratch);
            if !matches!(ask_in(&args, context), NativeRun::Fallback(_)) {
                found.push(format!("{name}: answered outside the subset"));
            }
            if snapshot(&scratch) != before {
                found.push(format!("{name}: changed something before handing back"));
            }
        };
        let plain = Context::default();
        for (name, extra) in [
            ("remote_src", json!({"remote_src": true})),
            ("directory_mode", json!({"directory_mode": "0755"})),
            ("content", json!({"content": "x"})),
            ("attributes", json!({"attributes": "+i"})),
            ("attr", json!({"attr": "+i"})),
            ("setype", json!({"setype": "tmp_t"})),
            ("unsafe_writes", json!({"unsafe_writes": true})),
            ("mode preserve", json!({"mode": "preserve"})),
            ("mode 0o", json!({"mode": "0o644"})),
            ("backup yes", json!({"backup": "yes"})),
            ("local_follow yes", json!({"local_follow": "yes"})),
            ("unknown argument", json!({"nope": 1})),
            ("two names", json!({"attributes": null, "attr": null})),
            ("relative dest", json!({"dest": "n.txt"})),
            ("repeated slashes", json!({"dest": format!("{t}//n.txt")})),
            ("no src", json!({"src": null})),
            ("src a directory", json!({"src": scratch.path("d")})),
            (
                "directories to create",
                json!({"dest": format!("{t}/new/"), "_original_basename": "n.txt"}),
            ),
            ("parent a file", json!({"dest": format!("{exe}/x")})),
            (
                "dest a directory",
                json!({"dest": format!("{t}/d"), "_original_basename": "sub.txt"}),
            ),
        ] {
            hand_back(name, extra, &plain);
        }
        let environment = Context {
            environment: [("LANG".to_string(), "C".to_string())].into(),
            ..Context::default()
        };
        hand_back("environment", json!({}), &environment);

        // The cases decided after the backup would be written or the link replaced, played on
        // the fixture file and on the link to it: a hand-back that came after either shows in
        // the snapshot as a backup file or a link turned into a file.
        let link = scratch.path("l");
        for dest in [&file, &link] {
            for (name, validate) in [
                ("validate expands", "test -s $HOME%s"),
                ("validate formats", "test %d %s"),
                ("validate comment", "test -s %s # x"),
                ("validate program missing", "volant-no-such-program %s"),
            ] {
                hand_back(
                    &format!("{name}, dest {dest}"),
                    json!({"dest": dest, "validate": validate}),
                    &plain,
                );
            }
        }

        // `preserved_copy` would give the backup the file's inode flags, through the link too.
        set_flags(&file, NODUMP);
        for dest in [&file, &link] {
            hand_back(
                &format!("nodump flag, dest {dest}"),
                json!({"dest": dest}),
                &plain,
            );
        }
        set_flags(&file, 0);

        let name = CString::new(file.clone()).unwrap();
        // SAFETY: the name and the value outlive the call.
        let set = unsafe {
            libc::setxattr(
                name.as_ptr(),
                c"user.volant".as_ptr(),
                b"1".as_ptr().cast(),
                1,
                0,
            )
        };
        assert_eq!(
            set, 0,
            "this file system takes no user xattrs, which the test needs"
        );
        for dest in [&file, &link] {
            hand_back(
                &format!("xattrs, dest {dest}"),
                json!({"dest": dest}),
                &plain,
            );
        }
        assert!(found.is_empty(), "{found:#?}");
    }

    /// `FS_NODUMP_FL`, which a file's owner may set.
    const NODUMP: libc::c_long = 0x40;

    /// `chattr =` the flags in `flags` (and the ones the file system keeps) on `path`.
    fn set_flags(path: &str, flags: libc::c_long) {
        let file = fs::File::open(path).unwrap();
        let fd = std::os::fd::AsRawFd::as_raw_fd(&file);
        let mut now: libc::c_long = 0;
        // SAFETY: both ioctls read or write one `long` through the pointer.
        let rc = unsafe {
            libc::ioctl(fd, libc::FS_IOC_GETFLAGS, &raw mut now);
            now = (now & AUTOMATIC) | flags;
            libc::ioctl(fd, libc::FS_IOC_SETFLAGS, &raw const now)
        };
        assert_eq!(
            rc, 0,
            "this file system takes no inode flags, which the test needs"
        );
    }

    /// The same device on the same mount is not a crossing; another device (a btrfs subvolume,
    /// on its parent's mount), another mount (a bind mount, same device), or a mount the kernel
    /// does not tell are.
    ///
    /// What would make this red: either comparison dropped, or an unknown mount trusted.
    #[test]
    fn a_rename_crosses_a_device_or_a_mount() {
        assert!(!crosses((1, Some(7)), (1, Some(7))));
        assert!(crosses((2, Some(7)), (1, Some(7))), "a subvolume");
        assert!(crosses((1, Some(8)), (1, Some(7))), "a bind mount");
        assert!(crosses((1, None), (1, Some(7))), "an unknown mount");
    }

    /// A staged source on another mount than the destination hands back with both untouched:
    /// `rename` would fail with EXDEV, and the reference would copy through a file of its own.
    /// `/dev/shm` is a tmpfs of its own on the machines this runs on; where it is not, the test
    /// fails saying so rather than checking nothing.
    ///
    /// What would make this red: the mount check dropped, which renames across and fails, or
    /// answers after the backup.
    #[test]
    fn a_source_on_another_mount_is_handed_back() {
        let scratch = scratch("copy-mount");
        let other = format!("/dev/shm/volant-copy-{}", std::process::id());
        fs::write(&other, "far\n").expect("/dev/shm is writable, which the test needs");
        let apart = mount_id(&other, false) != mount_id(&scratch.0, true);
        let dest = scratch.path("n.txt");
        write(&dest, "near\n", 0o644);
        let answer = ask(&json!({"src": other, "dest": dest, "backup": true}));
        let kept = fs::read_to_string(&other);
        let _ = fs::remove_file(&other);
        assert!(
            apart,
            "/dev/shm shares /var/tmp's mount here, which the test needs apart"
        );
        assert!(
            matches!(answer, NativeRun::Fallback(_)),
            "answered across mounts"
        );
        assert_eq!(kept.unwrap(), "far\n");
        assert_eq!(fs::read_to_string(&dest).unwrap(), "near\n");
        assert_eq!(
            fs::read_dir(&scratch.0).unwrap().count(),
            2,
            "a backup was made"
        );
    }

    /// A backup that outlives the task's deadline, or meets the controller's cancel, stops
    /// between two blocks.
    ///
    /// What would make this red: the backup copied in one go, without its clock.
    #[test]
    fn a_backup_stops_at_the_cancel() {
        let scratch = scratch("copy-backup-cancel");
        let big = scratch.path("big");
        write(&big, &"x".repeat(3 * 1024 * 1024), 0o644);
        let cancelled = || true;
        let clock = Clock {
            deadline: None,
            cancelled: &cancelled,
        };
        let copied = super::super::common::backup_copy(&big, &scratch.path("big~"), clock);
        assert!(
            matches!(copied, Err(BackupError::Stopped(Halt::Cancelled))),
            "{copied:?}"
        );
    }

    /// A `validate` that hangs ends with the task's `timeout`, answered as the Python path
    /// answers it, and with the controller's cancel.
    ///
    /// What would make this red: `validate` run without the task's clock, which waits out the
    /// sleep past the test's own limit.
    #[test]
    fn a_hung_validate_ends_with_the_timeout_or_the_cancel() {
        let scratch = scratch("copy-hung");
        let hang = scratch.path("hang");
        write(&hang, "#!/bin/sh\nexec sleep 600\n", 0o755);
        let args = json!({
            "src": stage(&scratch, "x\n"),
            "dest": scratch.path("n.txt"),
            "validate": format!("{hang} %s"),
        });
        let timed = args.clone();
        let answer = within(30, move || {
            let context = Context {
                timeout: Some(Duration::from_secs(1)),
                ..Context::default()
            };
            match ask_in(&timed, &context) {
                NativeRun::Done(result) => Some(result),
                _ => None,
            }
        });
        assert_eq!(answer, Some(TaskResult::timed_out(1)));

        let cancelled = Arc::new(AtomicBool::new(false));
        let flag = cancelled.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(300));
            flag.store(true, Ordering::Relaxed);
        });
        let answer = within(30, move || {
            let cancel = move || cancelled.load(Ordering::Relaxed);
            matches!(
                run(args.as_object().unwrap(), &Context::default(), &cancel),
                NativeRun::Cancelled
            )
        });
        assert!(answer, "the cancel did not stop the validate");
    }

    /// RFC 1321's own test suite, and the sum the reference gave `one\n`.
    ///
    /// What would make this red: any constant, rotation or padding rule of the MD5 above wrong.
    #[test]
    fn md5_matches_the_rfc() {
        let md5 = |text: &str| {
            let mut md5 = Md5::new();
            md5.update(text.as_bytes());
            md5.hex()
        };
        for (text, sum) in [
            ("", "d41d8cd98f00b204e9800998ecf8427e"),
            ("a", "0cc175b9c0f1b6a831c399e269772661"),
            ("abc", "900150983cd24fb0d6963f7d28e17f72"),
            ("message digest", "f96b697d7cb7938d525a2f31aaf161d0"),
            (
                "abcdefghijklmnopqrstuvwxyz",
                "c3fcd3d76192e4007dfb496cca67e13b",
            ),
            (
                "12345678901234567890123456789012345678901234567890123456789012345678901234567890",
                "57edf4a22be3c955ac49da2e2107b67a",
            ),
            ("one\n", "5bbf5a52328e7439ae6e719dfe712200"),
        ] {
            assert_eq!(md5(text), sum, "{text:?}");
        }
    }
}
