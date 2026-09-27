// SPDX-License-Identifier: GPL-3.0-or-later
//! `stat`, answered in the agent as `ansible/modules/stat.py` of ansible-core 2.19.12 answers.
//!
//! Hands back to the Python module when a task sets an `environment`, when the path is not a
//! plain absolute one, for a checksum other than SHA-1, when the owner or group of the path
//! cannot be told, or on any error the reference reports in words this file does not know.
//! `file` and `lsattr` run as the reference runs them, which is most of this native's time.

use std::collections::BTreeMap;
use std::fs::{self, Metadata};
use std::io::Read;
use std::os::unix::fs::MetadataExt;
use std::path::Path;

use serde_json::{Map, Value, json};

use super::common::{
    ArgSpec, Clock, Stop, access, bin_path, bool_param, check, check_names, clock, invocation,
    lookup_group, lookup_user, module_args, native_run, path_param, realpath, run_program,
    str_param, strerror,
};
use super::{Native, NativeRun};
use crate::modules::Context;

pub const NATIVE: Native = Native {
    name: "stat",
    aliases: &[],
    enabled: true,
    run,
};

const SPEC: &[ArgSpec] = &[
    ArgSpec {
        name: "path",
        aliases: &["dest", "name"],
        default: || Value::Null,
    },
    ArgSpec {
        name: "follow",
        aliases: &[],
        default: || Value::Bool(false),
    },
    ArgSpec {
        name: "get_checksum",
        aliases: &[],
        default: || Value::Bool(true),
    },
    ArgSpec {
        name: "get_mime",
        aliases: &["mime", "mime_type", "mime-type"],
        default: || Value::Bool(true),
    },
    ArgSpec {
        name: "get_attributes",
        aliases: &["attr", "attributes"],
        default: || Value::Bool(true),
    },
    ArgSpec {
        name: "checksum_algorithm",
        aliases: &["checksum", "checksum_algo"],
        default: || json!("sha1"),
    },
];

// The file type bits of `st_mode`, spelt out: `libc`'s are `u16` on macOS.
const S_IFMT: u32 = 0o170_000;
const S_IFREG: u32 = 0o100_000;
const S_IFLNK: u32 = 0o120_000;

/// `lsattr`'s letters and the names the reference gives them (`FILE_ATTRIBUTES`).
const FILE_ATTRIBUTES: &[(char, &str)] = &[
    ('A', "noatime"),
    ('a', "append"),
    ('c', "compressed"),
    ('C', "nocow"),
    ('d', "nodump"),
    ('D', "dirsync"),
    ('e', "extents"),
    ('E', "encrypted"),
    ('h', "blocksize"),
    ('i', "immutable"),
    ('I', "indexed"),
    ('j', "journalled"),
    ('N', "inline"),
    ('s', "zero"),
    ('S', "synchronous"),
    ('t', "notail"),
    ('T', "blockroot"),
    ('u', "undelete"),
    ('X', "compressedraw"),
    ('Z', "compresseddirty"),
];

/// `checksum_algorithm`'s `choices`, validated on every call whatever `get_checksum` says.
const CHECKSUMS: &[&str] = &["md5", "sha1", "sha224", "sha256", "sha384", "sha512"];

fn run(args: &Map<String, Value>, context: &Context, cancelled: &dyn Fn() -> bool) -> NativeRun {
    native_run(answer(args, context, clock(context, cancelled)), context)
}

fn answer(
    args: &Map<String, Value>,
    context: &Context,
    clock: Clock,
) -> Result<Map<String, Value>, Stop> {
    if !cfg!(target_os = "linux") {
        return Err("the native answers on Linux only".into());
    }
    if !context.environment.is_empty() {
        return Err("the task sets an environment".into());
    }
    check_names(SPEC, args)?;
    let params = module_args(SPEC, args);
    let path = path_param(&params, "path")?;
    let follow = bool_param(&params, "follow")?;
    let get_checksum = bool_param(&params, "get_checksum")?;
    let get_mime = bool_param(&params, "get_mime")?;
    let get_attributes = bool_param(&params, "get_attributes")?;
    // The reference fails its argument validation on anything else, taken or not.
    let algorithm = str_param(&params, "checksum_algorithm")?;
    if !algorithm.is_some_and(|name| CHECKSUMS.contains(&name)) {
        return Err("checksum_algorithm is not one of the reference's choices".into());
    }
    if get_checksum && algorithm != Some("sha1") {
        return Err("checksum_algorithm is not sha1".into());
    }

    let mut result = Map::new();
    let st = if follow {
        fs::metadata(path)
    } else {
        fs::symlink_metadata(path)
    };
    let stat = match st {
        Ok(st) => describe(path, &st, get_checksum, get_mime, get_attributes, clock)?,
        Err(err) if err.raw_os_error() == Some(libc::ENOENT) => json!({"exists": false}),
        Err(err) => {
            // `fail_json(msg=ex.strerror, exception=ex)`.
            let msg = err
                .raw_os_error()
                .and_then(strerror)
                .ok_or_else(|| Stop::HandBack(format!("stat of {path}: {err}")))?;
            result.insert("failed".into(), Value::Bool(true));
            result.insert("msg".into(), msg.into());
            result.insert("invocation".into(), invocation(SPEC, args));
            return Ok(result);
        }
    };
    result.insert("changed".into(), Value::Bool(false));
    result.insert("stat".into(), stat);
    result.insert("invocation".into(), invocation(SPEC, args));
    Ok(result)
}

/// The `stat` dictionary of a path that exists: `format_output`, then what `main` adds.
fn describe(
    path: &str,
    st: &Metadata,
    get_checksum: bool,
    get_mime: bool,
    get_attributes: bool,
    clock: Clock,
) -> Result<Value, Stop> {
    let mode = st.mode();
    let kind = mode & S_IFMT;
    let bit = |mask: u32| Value::Bool(mode & mask != 0);
    let time = |secs: i64, nanos: i64| json!(secs as f64 + nanos as f64 * 1e-9);
    let mut out = Map::new();
    out.insert("exists".into(), Value::Bool(true));
    out.insert("path".into(), path.into());
    out.insert("mode".into(), format!("{:04o}", mode & 0o7777).into());
    for (key, want) in [
        ("isdir", 0o040_000),
        ("ischr", 0o020_000),
        ("isblk", 0o060_000),
        ("isreg", S_IFREG),
        ("isfifo", 0o010_000),
        ("islnk", S_IFLNK),
        ("issock", 0o140_000),
    ] {
        out.insert(key.into(), Value::Bool(kind == want));
    }
    out.insert("uid".into(), st.uid().into());
    out.insert("gid".into(), st.gid().into());
    out.insert("size".into(), st.size().into());
    out.insert("inode".into(), st.ino().into());
    out.insert("dev".into(), st.dev().into());
    out.insert("nlink".into(), st.nlink().into());
    out.insert("atime".into(), time(st.atime(), st.atime_nsec()));
    out.insert("mtime".into(), time(st.mtime(), st.mtime_nsec()));
    out.insert("ctime".into(), time(st.ctime(), st.ctime_nsec()));
    for (key, mask) in [
        ("wusr", 0o200),
        ("rusr", 0o400),
        ("xusr", 0o100),
        ("wgrp", 0o020),
        ("rgrp", 0o040),
        ("xgrp", 0o010),
        ("woth", 0o002),
        ("roth", 0o004),
        ("xoth", 0o001),
        ("isuid", 0o4000),
        ("isgid", 0o2000),
    ] {
        out.insert(key.into(), bit(mask));
    }
    out.insert("blocks".into(), st.blocks().into());
    out.insert("block_size".into(), st.blksize().into());
    out.insert("device_type".into(), st.rdev().into());

    let readable = access(path, libc::R_OK);
    out.insert("readable".into(), readable.into());
    out.insert("writeable".into(), access(path, libc::W_OK).into());
    out.insert("executable".into(), access(path, libc::X_OK).into());

    if kind == S_IFLNK {
        out.insert("lnk_source".into(), realpath(path)?.into());
        let target = fs::read_link(path).map_err(|err| format!("readlink {path}: {err}"))?;
        let target = target
            .to_str()
            .ok_or_else(|| format!("the target of {path} is not UTF-8"))?;
        out.insert("lnk_target".into(), target.into());
    }
    if let Some(user) = lookup_user(&st.uid().to_string(), clock)? {
        out.insert("pw_name".into(), user.name.into());
    }
    if let Some(group) = lookup_group(&st.gid().to_string(), clock)? {
        out.insert("gr_name".into(), group.name.into());
    }
    if kind == S_IFREG && readable && get_checksum {
        out.insert("checksum".into(), sha1_of(path, clock)?);
    }
    if get_mime {
        let (mimetype, charset) = mime(path, clock)?;
        out.insert("mimetype".into(), mimetype.into());
        out.insert("charset".into(), charset.into());
    }
    if get_attributes {
        attributes(path, &mut out, clock)?;
    }
    Ok(Value::Object(out))
}

/// `digest_from_file(path, 'sha1')`: `None` when the path no longer exists.
///
/// The deadline and the cancel are looked at every 16 blocks (1 MiB): a disk image takes
/// minutes, and the Python module would be killed at the task's `timeout`.
fn sha1_of(path: &str, clock: Clock) -> Result<Value, Stop> {
    if fs::metadata(path).is_err() {
        return Ok(Value::Null);
    }
    let mut file = fs::File::open(path).map_err(|err| format!("opening {path}: {err}"))?;
    let mut sha1 = sha1_smol::Sha1::new();
    let mut block = vec![0; 64 * 1024];
    for round in 0_u64.. {
        if round % 16 == 0 {
            check(clock)?;
        }
        match file.read(&mut block) {
            Ok(0) => break,
            Ok(n) => sha1.update(&block[..n]),
            Err(err) => return Err(Stop::HandBack(format!("reading {path}: {err}"))),
        }
    }
    Ok(sha1.digest().to_string().into())
}

/// `file --mime-type --mime-encoding`, read as the reference reads it: `unknown` for whatever
/// it cannot take apart, and when there is no `file`.
fn mime(path: &str, clock: Clock) -> Result<(String, String), Stop> {
    let mut mimetype = "unknown".to_string();
    let mut charset = "unknown".to_string();
    let Some(file) = bin_path("file") else {
        return Ok((mimetype, charset));
    };
    let Some(out) = run_command(&file, &["--mime-type", "--mime-encoding", path], clock)? else {
        return Ok((mimetype, charset));
    };
    // `mimetype, charset = out.rsplit(':', 1)[1].split(';')`, then `charset.split('=')[1]`.
    if let Some((_, tail)) = out.rsplit_once(':') {
        let parts: Vec<&str> = tail.split(';').collect();
        if let [kind, encoding] = parts[..] {
            mimetype = kind.trim().to_string();
            if let Some(value) = encoding.split('=').nth(1) {
                charset = value.trim().to_string();
            }
        }
    }
    Ok((mimetype, charset))
}

/// `get_file_attributes(path)` into `out`: `lsattr -vd`, its first word the version and its
/// second the flags.
fn attributes(path: &str, out: &mut Map<String, Value>, clock: Clock) -> Result<(), Stop> {
    out.insert("version".into(), Value::Null);
    out.insert("attributes".into(), json!([]));
    out.insert("attr_flags".into(), "".into());
    let Some(lsattr) = bin_path("lsattr") else {
        return Ok(());
    };
    let Some(text) = run_command(&lsattr, &["-vd", path], clock)? else {
        return Ok(());
    };
    let mut words = text.split_whitespace();
    if let Some(version) = words.next() {
        out.insert("version".into(), version.into());
    }
    if let Some(flags) = words.next() {
        let flags = flags.replace('-', "");
        let names: Vec<Value> = flags
            .chars()
            .filter_map(|c| FILE_ATTRIBUTES.iter().find(|(f, _)| *f == c))
            .map(|(_, name)| Value::from(*name))
            .collect();
        out.insert("attr_flags".into(), flags.into());
        out.insert("attributes".into(), names.into());
    }
    Ok(())
}

/// `run_command`'s standard output when the command exits 0, `None` otherwise, run through
/// `command`'s executor under the task's clock. A command that cannot start fails the
/// reference's module outright; the native hands back instead.
fn run_command(program: &str, args: &[&str], clock: Clock) -> Result<Option<String>, Stop> {
    match run_program(&BTreeMap::new(), clock, Path::new(program), args)? {
        Some((0, out)) => Ok(Some(out)),
        Some(_) => Ok(None),
        None => Err(Stop::HandBack(format!("{program} cannot be run"))),
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use serde_json::{Value, json};

    use volant_protocol::TaskResult;

    use super::super::common::golden::{Scratch, differences, within};
    use super::*;

    /// The index's volatile keys, plus the `lsattr` ones: the flags depend on the file system.
    const VOLATILE: &[&str] = &[
        "stat.atime",
        "stat.mtime",
        "stat.ctime",
        "stat.inode",
        "stat.dev",
        "stat.version",
        "stat.attr_flags",
        "stat.attributes",
    ];

    fn ask(args: &Value, context: &Context) -> NativeRun {
        run(args.as_object().unwrap(), context, &|| false)
    }

    /// Every recorded `stat` case, on the fixture the golden play builds: a file, the directory,
    /// a link followed and not, a missing path, the arguments the `copy` action sends, and a stat
    /// with nothing extra asked.
    ///
    /// What would make this red: `follow` ignored (the link cases trade their `islnk`, `mode` and
    /// `lnk_*` keys), a mode not zero-padded to four digits, the checksum taken of a directory or
    /// of a link not followed, `file` given `-L` (the followed link's `mimetype` is
    /// `inode/symlink` in the reference), a key missing or extra (`pw_name`, `version`,
    /// `device_type`), or `invocation` without the defaults.
    #[test]
    fn stat_answers_every_recorded_case_like_the_reference() {
        let scratch = Scratch::new("stat");
        scratch.fixture();
        let cases = [
            (
                "stat-file",
                include_str!("../../../volant/tests/golden/native/stat-file.json"),
                json!({"path": "<golden-tmp>/f.txt"}),
            ),
            (
                "stat-dir",
                include_str!("../../../volant/tests/golden/native/stat-dir.json"),
                json!({"path": "<golden-tmp>"}),
            ),
            (
                "stat-link-follow",
                include_str!("../../../volant/tests/golden/native/stat-link-follow.json"),
                json!({"path": "<golden-tmp>/l", "follow": true}),
            ),
            (
                "stat-link-nofollow",
                include_str!("../../../volant/tests/golden/native/stat-link-nofollow.json"),
                json!({"path": "<golden-tmp>/l"}),
            ),
            (
                "stat-missing",
                include_str!("../../../volant/tests/golden/native/stat-missing.json"),
                json!({"path": "<golden-tmp>/nope"}),
            ),
            (
                "stat-plugin",
                include_str!("../../../volant/tests/golden/native/stat-plugin.json"),
                json!({"path": "<golden-tmp>/f.txt", "follow": false, "get_checksum": true,
                       "checksum_algorithm": "sha1"}),
            ),
            (
                "stat-bare",
                include_str!("../../../volant/tests/golden/native/stat-bare.json"),
                json!({"path": "<golden-tmp>/f.txt", "get_checksum": false, "get_mime": false,
                       "get_attributes": false}),
            ),
        ];
        let mut found = Vec::new();
        for (case, recording, args) in cases {
            let args: Value =
                serde_json::from_str(&args.to_string().replace("<golden-tmp>", &scratch.0))
                    .unwrap();
            match ask(&args, &Context::default()) {
                NativeRun::Done(result) => found.extend(
                    differences(recording, result.0, &scratch, VOLATILE)
                        .into_iter()
                        .map(|d| format!("{case}: {d}")),
                ),
                NativeRun::Fallback(why) => found.push(format!("{case}: handed back, {why}")),
                NativeRun::Cancelled => found.push(format!("{case}: cancelled")),
            }
        }
        assert!(found.is_empty(), "{found:#?}");
    }

    /// A checksum of a disk image stops at the task's `timeout` with the answer the Python path
    /// gives, and at the controller's cancel: a 64 GiB sparse file, which SHA-1 takes minutes
    /// over.
    ///
    /// What would make this red: the loop not looking at the clock (the test runs for minutes
    /// and nextest ends it), or the timeout answered as anything else.
    #[test]
    fn a_long_checksum_stops_at_the_timeout_and_the_cancel() {
        let scratch = Scratch::new("stat-sparse");
        let image = scratch.path("disk.img");
        fs::File::create(&image).unwrap().set_len(64 << 30).unwrap();
        let args = json!({"path": image, "get_mime": false, "get_attributes": false});
        let context = Context {
            timeout: Some(std::time::Duration::from_secs(1)),
            ..Context::default()
        };
        let timed = args.clone();
        let NativeRun::Done(result) = within(30, move || ask(&timed, &context)) else {
            panic!("no answer")
        };
        assert_eq!(result, TaskResult::timed_out(1));
        let cancelled = within(30, move || {
            run(args.as_object().unwrap(), &Context::default(), &|| true)
        });
        assert!(matches!(cancelled, NativeRun::Cancelled));
    }

    /// Outside the subset the task goes to the Python module: another checksum, a task
    /// `environment`, a boolean the reference would convert, an option given twice, a path the
    /// reference would expand.
    ///
    /// What would make this red: any of those answered, each of which the reference answers
    /// differently (another digest, `PATH` for `file` and `lsattr`, `invocation` showing the
    /// converted value, a warning, another path).
    #[test]
    fn stat_hands_back_what_it_cannot_answer() {
        let scratch = Scratch::new("stat-back");
        let file = scratch.fixture();
        let mut environment = Context::default();
        environment
            .environment
            .insert("PATH".into(), "/nowhere".into());
        for (args, context) in [
            (
                json!({"path": file, "checksum_algorithm": "md5"}),
                Context::default(),
            ),
            (json!({"path": file}), environment),
            (json!({"path": file, "follow": "yes"}), Context::default()),
            (json!({"path": file, "dest": file}), Context::default()),
            (json!({"path": "~/f.txt"}), Context::default()),
            (json!({"path": "$HOME/f.txt"}), Context::default()),
            (
                json!({"path": file, "get_checksum": false, "checksum_algorithm": "sha3"}),
                Context::default(),
            ),
            (
                json!({"path": file, "get_checksum": false, "checksum": "SHA256"}),
                Context::default(),
            ),
            (
                json!({"path": file, "get_checksum": false, "checksum_algorithm": 1}),
                Context::default(),
            ),
        ] {
            assert!(
                matches!(ask(&args, &context), NativeRun::Fallback(_)),
                "{args}"
            );
        }
        assert!(matches!(
            ask(
                &json!({"path": file, "checksum_algorithm": "md5", "get_checksum": false}),
                &Context::default()
            ),
            NativeRun::Done(_)
        ));
    }
}
