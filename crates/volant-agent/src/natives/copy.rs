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
#[expect(clippy::too_many_lines)]
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
        .ok_or_else(|| Stop::Fail(message("validate names no program".into())))?;
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
mod tests;
