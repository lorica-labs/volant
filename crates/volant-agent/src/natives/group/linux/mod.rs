// SPDX-License-Identifier: GPL-3.0-or-later

#[cfg(all(test, target_os = "linux"))]
use std::collections::BTreeMap;
use std::path::PathBuf;

use serde_json::{Map, Value};

use super::NativeRun;
use crate::modules::Context;
#[cfg(all(test, target_os = "linux"))]
use crate::natives::common::files_first;
use crate::natives::common::{
    ArgSpec, bool_param, check_names, clock, invocation, lookup_group, native_run, str_param,
};
use crate::natives::runtime::{Clock, Stop};

mod helpers;

pub use helpers::{
    bin, entry, failure, host_gate, id_param, outcome, outside_subset, plain_name, present,
    resolve_group, run_command,
};

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

/// Where the C library reads which sources answer for accounts and groups.
pub const NSSWITCH: &str = "/etc/nsswitch.conf";

/// `group`'s `argument_spec` in ansible-core 2.19.12.
const SPEC: &[ArgSpec] = &[
    ArgSpec {
        name: "state",
        aliases: &[],
        default: || Value::from("present"),
    },
    ArgSpec {
        name: "name",
        aliases: &[],
        default: || Value::Null,
    },
    ArgSpec {
        name: "force",
        aliases: &[],
        default: || Value::Bool(false),
    },
    ArgSpec {
        name: "gid",
        aliases: &[],
        default: || Value::Null,
    },
    ArgSpec {
        name: "system",
        aliases: &[],
        default: || Value::Bool(false),
    },
    ArgSpec {
        name: "local",
        aliases: &[],
        default: || Value::Bool(false),
    },
    ArgSpec {
        name: "non_unique",
        aliases: &[],
        default: || Value::Bool(false),
    },
    ArgSpec {
        name: "gid_min",
        aliases: &[],
        default: || Value::Null,
    },
    ArgSpec {
        name: "gid_max",
        aliases: &[],
        default: || Value::Null,
    },
];

/// The arguments the native answers for; any other one given hands the task back.
const SUBSET: &[&str] = &["name", "state", "gid", "system"];

struct Request<'a> {
    name: &'a str,
    present: bool,
    gid: Option<u32>,
    system: bool,
}

fn request<'a>(
    args: &Map<String, Value>,
    params: &'a Map<String, Value>,
) -> Result<Request<'a>, String> {
    check_names(SPEC, args)?;
    outside_subset(args, SUBSET)?;
    let name = str_param(params, "name")?
        .filter(|name| plain_name(name))
        .ok_or("the module reads this group name differently")?;
    Ok(Request {
        name,
        present: present(params)?,
        gid: id_param(params, "gid")?,
        system: bool_param(params, "system")?,
    })
}

/// The group named `name` as the module finds it through `grp`, from `/etc/group`: `None`
/// when the host has no such group. A group only the name service knows hands back: the
/// commands would change the files, not the directory.
fn find_group(name: &str, clock: Clock) -> Result<Option<u32>, Stop> {
    let known = lookup_group(name, clock)?;
    let local = entry("/etc/group", 4, name)?.map(|fields| fields[2].parse::<u32>().unwrap());
    match (known, local) {
        (None, None) => Ok(None),
        (Some(known), Some(gid)) if known.gid == gid => Ok(Some(gid)),
        (Some(_), None) => Err("the name service knows this group and /etc/group does not".into()),
        _ => Err("/etc/group and the name service disagree about this group".into()),
    }
}

fn answer(
    args: &Map<String, Value>,
    context: &Context,
    clock: Clock,
    nsswitch: &str,
) -> Result<Map<String, Value>, Stop> {
    let invocation = invocation(SPEC, args);
    let params = invocation["module_args"].as_object().unwrap();
    let request = request(args, params)?;
    host_gate(nsswitch)?;
    let found = find_group(request.name, clock)?;
    let name = request.name;
    // Decided, and the command found, before anything runs.
    let command: Option<(PathBuf, Vec<String>)> = match (request.present, found) {
        (false, None) => None,
        (false, Some(_)) => Some((bin(context, "groupdel")?, vec![name.to_string()])),
        (true, None) => {
            let mut argv = Vec::new();
            if let Some(gid) = request.gid {
                argv.extend(["-g".to_string(), gid.to_string()]);
            }
            if request.system {
                argv.push("-r".into());
            }
            argv.push(name.to_string());
            Some((bin(context, "groupadd")?, argv))
        }
        (true, Some(gid)) => {
            let groupmod = bin(context, "groupmod")?;
            request.gid.filter(|wanted| *wanted != gid).map(|wanted| {
                (
                    groupmod,
                    vec!["-g".into(), wanted.to_string(), name.to_string()],
                )
            })
        }
    };
    let ran = match command {
        Some((program, argv)) => {
            let argv: Vec<&str> = argv.iter().map(String::as_str).collect();
            let ran = run_command(context, clock, &program, &argv)?;
            if ran.0 != 0 {
                return Ok(failure(
                    invocation.clone(),
                    &[("name", Value::from(name)), ("msg", Value::from(ran.2))],
                ));
            }
            Some(ran)
        }
        None => None,
    };
    let mut result = Map::new();
    result.insert("name".into(), Value::from(name));
    result.insert("state".into(), params["state"].clone());
    outcome(&mut result, ran.as_ref());
    match entry("/etc/group", 4, name) {
        Ok(Some(fields)) => {
            result.insert("system".into(), Value::Bool(request.system));
            result.insert("gid".into(), Value::from(fields[2].parse::<u32>().unwrap()));
        }
        Ok(None) => {}
        Err(reason) => return Ok(failure(invocation.clone(), &[("msg", Value::from(reason))])),
    }
    result.insert("invocation".into(), invocation.clone());
    Ok(result)
}

#[cfg(all(test, target_os = "linux"))]
pub mod tests {
    pub(crate) use crate::testing::args;
    use serde_json::json;

    use super::*;
    use crate::natives::common::golden::{Scratch, fake_getent};
    use crate::natives::runtime::unbounded;

    /// Fake commands in a directory of their own, first on the context's `PATH`: each writes
    /// its arguments, one per line, to `<name>.argv`, prints `out` and `err`, and exits `rc`.
    /// Asked `--help`, each lists `-a, --append` as `usermod` does, and records nothing.
    pub struct Fakes {
        pub scratch: Scratch,
        pub context: Context,
    }

    impl Fakes {
        pub fn new(test: &str, names: &[&str], rc: i32) -> Fakes {
            use std::os::unix::fs::PermissionsExt;
            let scratch = Scratch::new(test);
            let bin = scratch.path("fakes");
            std::fs::create_dir(&bin).unwrap();
            for name in names {
                let path = format!("{bin}/{name}");
                std::fs::write(
                    &path,
                    format!(
                        "#!/bin/sh\n[ \"$1\" = --help ] && echo '  -a, --append  append' && exit 0\nprintf '%s\\n' \"$@\" > {bin}/{name}.argv\necho out\necho err >&2\nexit {rc}\n"
                    ),
                )
                .unwrap();
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
            }
            let path = format!("{bin}:{}", std::env::var("PATH").unwrap_or_default());
            let context = Context {
                environment: BTreeMap::from([("PATH".to_string(), path)]),
                ..Context::default()
            };
            // The commands the native runs must be these, never the host's own.
            for name in names {
                assert_eq!(bin_of(&context, name), format!("{bin}/{name}"));
            }
            Fakes { scratch, context }
        }

        /// A fake `name` running `body` (`sh`), recording nothing.
        pub fn script(&self, name: &str, body: &str) {
            use std::os::unix::fs::PermissionsExt;
            let path = self.scratch.path(&format!("fakes/{name}"));
            std::fs::write(
                &path,
                format!(
                    "#!/bin/sh
{body}
"
                ),
            )
            .unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }

        /// Forgets that `name` ran.
        pub fn clear(&self, name: &str) {
            let _ = std::fs::remove_file(self.scratch.path(&format!("fakes/{name}.argv")));
        }

        /// On a host outside the natives' subset (not Debian or Ubuntu, SELinux on, another
        /// name service before `files`), checks the documented hand-back through `ask` and
        /// says so: the caller then stops, having checked what this host allows.
        pub fn outside(&self, ask: Ask) -> bool {
            let Err(reason) = host_gate(NSSWITCH) else {
                return false;
            };
            let answer = ask(&args(json!({"name": "root"})), &self.context);
            assert!(
                matches!(&answer, Err(Stop::HandBack(why)) if *why == reason),
                "a host outside the subset ({reason}) must hand back"
            );
            eprintln!("this host is outside the natives' subset: {reason}");
            true
        }

        /// The arguments `name` was run with, or `None` when it never ran.
        pub fn argv(&self, name: &str) -> Option<Vec<String>> {
            let text =
                std::fs::read_to_string(self.scratch.path(&format!("fakes/{name}.argv"))).ok()?;
            Some(text.lines().map(str::to_string).collect())
        }
    }

    /// A native's answer to a task on this host's own `nsswitch.conf`.
    pub type Ask = fn(&Map<String, Value>, &Context) -> Result<Map<String, Value>, Stop>;

    fn ask(args: &Map<String, Value>, context: &Context) -> Result<Map<String, Value>, Stop> {
        answer(args, context, unbounded(), NSSWITCH)
    }

    fn bin_of(context: &Context, name: &str) -> String {
        bin(context, name).unwrap().display().to_string()
    }

    pub fn scratch_name(prefix: &str) -> String {
        format!("{prefix}{}", std::process::id())
    }

    /// `groupadd` takes the module's arguments in its order; an existing group is modified only where it differs.
    #[test]
    fn groupadd_and_groupmod_follow_the_module() {
        {
            let fakes = Fakes::new("groupadd", &["groupadd"], 0);
            if fakes.outside(ask) {
                return;
            }
            let name = scratch_name("volantg");
            let mut answer = ask(
                &args(json!({"name": name, "gid": 64998, "system": true})),
                &fakes.context,
            )
            .unwrap();
            assert_eq!(
                fakes.argv("groupadd").unwrap(),
                ["-g", "64998", "-r", name.as_str()]
            );
            assert_eq!(answer["invocation"]["module_args"]["gid_min"], Value::Null);
            answer.remove("invocation");
            assert_eq!(
                Value::Object(answer),
                json!({"name": name, "state": "present", "changed": true,
                   "stdout": "out\n", "stderr": "err\n"})
            );
        }
        {
            let fakes = Fakes::new("groupmod", &["groupmod", "groupdel"], 0);
            if fakes.outside(ask) {
                return;
            }
            let same = ask(&args(json!({"name": "root", "gid": 0})), &fakes.context).unwrap();
            assert_eq!(fakes.argv("groupmod"), None);
            assert_eq!(same["changed"], false);
            assert_eq!(same["gid"], 0);
            assert_eq!(same["system"], false);
            assert!(!same.contains_key("stdout"));

            let moved = ask(&args(json!({"name": "root", "gid": 5})), &fakes.context).unwrap();
            assert_eq!(fakes.argv("groupmod").unwrap(), ["-g", "5", "root"]);
            assert_eq!(moved["changed"], true);
            // Read back after the command, which here changed nothing.
            assert_eq!(moved["gid"], 0);

            let gone = ask(
                &args(json!({"name": "root", "state": "absent"})),
                &fakes.context,
            )
            .unwrap();
            assert_eq!(fakes.argv("groupdel").unwrap(), ["root"]);
            assert_eq!(gone["changed"], true);
            assert_eq!(gone["state"], "absent");
        }
        {
            let fakes = Fakes::new("groupadd-fails", &["groupadd"], 9);
            if fakes.outside(ask) {
                return;
            }
            let name = scratch_name("volantf");
            let mut answer = ask(&args(json!({"name": name})), &fakes.context).unwrap();
            assert!(answer.remove("invocation").is_some());
            assert_eq!(
                Value::Object(answer),
                json!({"failed": true, "name": name, "msg": "err\n"})
            );
        }
    }

    /// An argument outside the subset, or a group only the name service knows, hands back first.
    #[test]
    fn what_is_outside_the_subset_hands_back_before_any_command() {
        {
            let fakes = Fakes::new("group-subset", &["groupadd", "groupdel"], 0);
            let name = scratch_name("volants");
            for (key, value) in [
                ("local", json!(true)),
                ("non_unique", json!(true)),
                ("force", json!(true)),
                ("gid_min", json!(1000)),
                ("gid_max", json!(2000)),
            ] {
                let mut given = args(json!({"name": name, "gid": 64997}));
                given.insert(key.into(), value);
                let answer = ask(&given, &fakes.context);
                assert!(
                    matches!(answer, Err(Stop::HandBack(_))),
                    "{key} was answered"
                );
            }
            assert_eq!(fakes.argv("groupadd"), None);
        }
        {
            let fakes = Fakes::new("group-nss", &["groupadd", "groupdel"], 0);
            if fakes.outside(ask) {
                return;
            }
            let name = scratch_name("volantn");
            let _path = fake_getent(
                &fakes.scratch,
                &format!("[ \"$2\" = {name} ] && echo '{name}:x:5000:' && exit 0; exit 2"),
            );
            for state in ["present", "absent"] {
                let answer = ask(&args(json!({"name": name, "state": state})), &fakes.context);
                assert!(
                    matches!(answer, Err(Stop::HandBack(_))),
                    "{state} was answered"
                );
            }
            assert_eq!(fakes.argv("groupadd"), None);
            assert_eq!(fakes.argv("groupdel"), None);
        }
        {
            let fakes = Fakes::new("group-nsswitch", &["groupmod"], 0);
            if fakes.outside(ask) {
                return;
            }
            let conf = fakes.scratch.path("nsswitch.conf");
            for text in [
                "passwd: files\ngroup: sss files\n",
                "passwd: sss files\ngroup: files\n",
            ] {
                std::fs::write(&conf, text).unwrap();
                let answer = answer(
                    &args(json!({"name": "root", "gid": 5})),
                    &fakes.context,
                    unbounded(),
                    &conf,
                );
                assert!(
                    matches!(answer, Err(Stop::HandBack(_))),
                    "{text:?} was answered"
                );
            }
            assert_eq!(fakes.argv("groupmod"), None);
            std::fs::write(&conf, "# sss\npasswd: compat\ngroup:\tfiles systemd\n").unwrap();
            let args = args(json!({"name": "root", "gid": 5}));
            answer(&args, &fakes.context, unbounded(), &conf).unwrap();
            assert_eq!(fakes.argv("groupmod").unwrap(), ["-g", "5", "root"]);
            assert!(files_first(&fakes.scratch.path("none"), "group"));
        }
    }
}
