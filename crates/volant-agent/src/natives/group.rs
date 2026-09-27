// SPDX-License-Identifier: GPL-3.0-or-later
//! `group`, answered in the agent: ansible-core 2.19.12's `group.py`, its `Linux` class, for
//! `name`, `state`, `gid` and `system`. It reads `/etc/group`, runs the module's `groupadd`,
//! `groupmod` or `groupdel` with the module's arguments in the module's order, and answers with
//! the module's keys, `stdout` and `stderr` included when the command printed them.
//!
//! Handed to the Python module before anything changes: any other argument (`local`,
//! `non_unique`, `force`, `gid_min`, `gid_max`), a value the module would convert or refuse, a
//! host outside Debian and Ubuntu (the module takes another class on Alpine), a missing command,
//! and a group the name service knows while `/etc/group` does not hold it (LDAP, sssd).
//!
//! The helpers `user` shares live here too: the host gate, the commands, the account files.

use super::{Native, NativeRun};

pub const NATIVE: Native = Native {
    name: "group",
    aliases: &[],
    enabled: cfg!(unix),
    run,
};

#[cfg(not(unix))]
mod linux {
    use serde_json::{Map, Value};

    use super::NativeRun;
    use crate::modules::Context;

    pub fn run(_: &Map<String, Value>, _: &Context, _: &dyn Fn() -> bool) -> NativeRun {
        NativeRun::Fallback("the native group changes a Linux host".into())
    }
}

pub(super) use linux::*;

#[cfg(unix)]
mod linux {
    use std::collections::BTreeMap;
    use std::path::{Path, PathBuf};

    use serde_json::{Map, Value};

    use super::NativeRun;
    use crate::modules::Context;
    use crate::natives::common::{
        Account, ArgSpec, Clock, Group, Stop, bool_param, check_names, clock, group_account,
        invocation, lookup_group, native_run, str_param,
    };
    use crate::natives::setup::{self, Root};

    pub fn run(
        args: &Map<String, Value>,
        context: &Context,
        cancelled: &dyn Fn() -> bool,
    ) -> NativeRun {
        native_run(answer(args, context, clock(context, cancelled)), context)
    }

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

    /// Hands back on the first argument outside `subset`, before anything is read or run.
    pub fn outside_subset(args: &Map<String, Value>, subset: &[&str]) -> Result<(), String> {
        match args.keys().find(|key| !subset.contains(&key.as_str())) {
            Some(key) => Err(format!("{key} is left to the module")),
            None => Ok(()),
        }
    }

    /// `state`, one of the module's two choices.
    pub fn present(params: &Map<String, Value>) -> Result<bool, String> {
        match params["state"].as_str() {
            Some("present") => Ok(true),
            Some("absent") => Ok(false),
            _ => Err("state is not one of the module's choices".into()),
        }
    }

    /// A `type='int'` id taken as a JSON integer in range; the module converts strings and
    /// floats, and passes a negative or huge one on to a command that refuses it.
    pub fn id_param(params: &Map<String, Value>, name: &str) -> Result<Option<u32>, String> {
        match &params[name] {
            Value::Null => Ok(None),
            value => value
                .as_u64()
                .and_then(|id| u32::try_from(id).ok())
                .map(Some)
                .ok_or_else(|| format!("{name} is not an id the native takes")),
        }
    }

    /// A name the account files and the commands read as itself. All digits would be looked up
    /// as an id by the shared lookups.
    pub fn plain_name(name: &str) -> bool {
        !name.is_empty()
            && !name.contains([':', ',', '\n', '\0'])
            && !name.bytes().all(|b| b.is_ascii_digit())
    }

    /// The host checks made before anything else: a Debian or Ubuntu host, where the module
    /// takes its generic `User` and its `Linux` group class, with SELinux off.
    pub fn host_gate() -> Result<(), String> {
        let os_release = std::fs::read_to_string("/etc/os-release")
            .map_err(|err| format!("reading /etc/os-release: {err}"))?;
        let id = os_release
            .lines()
            .filter_map(|line| line.strip_prefix("ID="))
            .next_back()
            .map(|id| id.trim_matches(['"', '\'']).to_lowercase());
        if !matches!(id.as_deref(), Some("debian" | "ubuntu")) {
            return Err("the distribution is outside Debian and Ubuntu".into());
        }
        if crate::natives::common::selinux_enabled() {
            return Err("SELinux is on".into());
        }
        Ok(())
    }

    /// `module.get_bin_path(name, required=True)` along the module's `PATH`. A missing command
    /// fails the module; the native hands back instead.
    pub fn bin(context: &Context, name: &str) -> Result<PathBuf, String> {
        let path = context
            .environment
            .get("PATH")
            .cloned()
            .or_else(|| std::env::var("PATH").ok())
            .unwrap_or_default();
        let path = BTreeMap::from([("PATH".to_string(), path)]);
        setup::bin_path(&Root::real(), &path, name)
            .ok_or_else(|| format!("the module finds no {name}"))
    }

    /// `module.run_command(argv)` under the task's clock: exit code, output, error output. A
    /// command that cannot be started changed nothing, so the task is handed back.
    pub fn run_command(
        context: &Context,
        clock: Clock,
        program: &Path,
        args: &[&str],
    ) -> Result<(i32, String, String), Stop> {
        setup::run_output(&context.environment, clock, program, args)?
            .map_err(|_| Stop::HandBack(format!("{} could not be started", program.display())))
    }

    /// The first entry of the colon-separated file `file` named `name`, as the C library's
    /// `files` source finds it: a line with fewer than `fields` fields or a non-numeric id is
    /// skipped. `None` when no entry has that name.
    pub fn entry(file: &str, fields: usize, name: &str) -> Result<Option<Vec<String>>, String> {
        let text = std::fs::read_to_string(file).map_err(|err| format!("reading {file}: {err}"))?;
        for line in text.lines() {
            if line.starts_with(['+', '-']) {
                return Err(format!("{file} has NIS compat entries"));
            }
            let parts: Vec<&str> = line.split(':').collect();
            if parts.len() >= fields && parts[0] == name && parts[2].parse::<u32>().is_ok() {
                return Ok(Some(parts.into_iter().map(str::to_string).collect()));
            }
        }
        Ok(None)
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
            (Some(_), None) => {
                Err("the name service knows this group and /etc/group does not".into())
            }
            _ => Err("/etc/group and the name service disagree about this group".into()),
        }
    }

    /// `group_exists`/`group_info` of `user.py` for a `group` or `groups` argument: by gid when
    /// Python's `int()` reads it, by name otherwise. `None` when there is no such group, which
    /// the module fails on; anything Python would read differently hands back.
    pub fn resolve_group(given: &str, clock: Clock) -> Result<Option<Group>, Stop> {
        match group_account(given, clock)? {
            Account::Unknown(_) => Ok(None),
            Account::Id(_) if given.bytes().all(|b| b.is_ascii_digit()) => {
                lookup_group(given, clock)?.map(Some).ok_or_else(|| {
                    Stop::HandBack(
                        "no group has this id; the module then looks it up as a name".into(),
                    )
                })
            }
            Account::Id(gid) => Ok(Some(Group {
                name: given.to_string(),
                gid,
            })),
        }
    }

    /// `rc`, `out` and `err` as the module's `main` ends with them: `changed`, and the outputs
    /// only when not empty.
    pub fn outcome(result: &mut Map<String, Value>, ran: Option<&(i32, String, String)>) {
        result.insert("changed".into(), Value::Bool(ran.is_some()));
        if let Some((_, out, err)) = ran {
            for (key, text) in [("stdout", out), ("stderr", err)] {
                if !text.is_empty() {
                    result.insert(key.into(), Value::from(text.as_str()));
                }
            }
        }
    }

    /// `module.fail_json(**keys)`.
    pub fn failure(invocation: Value, keys: &[(&str, Value)]) -> Map<String, Value> {
        let mut result: Map<String, Value> = keys
            .iter()
            .map(|(k, v)| ((*k).to_string(), v.clone()))
            .collect();
        result.insert("failed".into(), Value::Bool(true));
        result.insert("invocation".into(), invocation);
        result
    }

    fn answer(
        args: &Map<String, Value>,
        context: &Context,
        clock: Clock,
    ) -> Result<Map<String, Value>, Stop> {
        let invocation = invocation(SPEC, args);
        let params = invocation["module_args"].as_object().unwrap();
        let request = request(args, params)?;
        host_gate()?;
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
        use serde_json::json;

        use super::*;
        use crate::natives::common::golden::{Scratch, fake_getent};
        use crate::natives::setup::unbounded;

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
                    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
                        .unwrap();
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

            /// The arguments `name` was run with, or `None` when it never ran.
            pub fn argv(&self, name: &str) -> Option<Vec<String>> {
                let text =
                    std::fs::read_to_string(self.scratch.path(&format!("fakes/{name}.argv")))
                        .ok()?;
                Some(text.lines().map(str::to_string).collect())
            }
        }

        fn bin_of(context: &Context, name: &str) -> String {
            bin(context, name).unwrap().display().to_string()
        }

        pub fn args(value: Value) -> Map<String, Value> {
            value.as_object().unwrap().clone()
        }

        pub fn scratch_name(prefix: &str) -> String {
            format!("{prefix}{}", std::process::id())
        }

        /// A group the host does not have is created with `groupadd -g <gid> -r <name>`, the
        /// module's order, and answered with the module's keys.
        ///
        /// What would make this red: `-r` before `-g`, or the name anywhere but last; a result
        /// without the command's `stdout`/`stderr`, or with `gid`/`system` for a group the host
        /// still does not hold.
        #[test]
        fn groupadd_takes_the_module_s_arguments_in_its_order() {
            let fakes = Fakes::new("groupadd", &["groupadd"], 0);
            let name = scratch_name("volantg");
            let answer = answer(
                &args(json!({"name": name, "gid": 64998, "system": true})),
                &fakes.context,
                unbounded(),
            )
            .unwrap();
            assert_eq!(
                fakes.argv("groupadd").unwrap(),
                ["-g", "64998", "-r", name.as_str()]
            );
            assert_eq!(answer["changed"], true);
            assert_eq!(answer["stdout"], "out\n");
            assert_eq!(answer["stderr"], "err\n");
            assert_eq!(answer["state"], "present");
            assert!(!answer.contains_key("gid") && !answer.contains_key("system"));
            assert_eq!(answer["invocation"]["module_args"]["gid_min"], Value::Null);
        }

        /// A group already holding the gid asked for runs nothing; another gid runs
        /// `groupmod -g <gid> <name>`; `absent` runs `groupdel <name>`.
        ///
        /// What would make this red: `groupmod` run for an unchanged gid (`changed: true`), or
        /// run with the name first; `groupdel` given anything but the name.
        #[test]
        fn an_existing_group_is_modified_only_where_it_differs() {
            let fakes = Fakes::new("groupmod", &["groupmod", "groupdel"], 0);
            let same = answer(
                &args(json!({"name": "root", "gid": 0})),
                &fakes.context,
                unbounded(),
            )
            .unwrap();
            assert_eq!(fakes.argv("groupmod"), None);
            assert_eq!(same["changed"], false);
            assert_eq!(same["gid"], 0);
            assert_eq!(same["system"], false);
            assert!(!same.contains_key("stdout"));

            let moved = answer(
                &args(json!({"name": "root", "gid": 5})),
                &fakes.context,
                unbounded(),
            )
            .unwrap();
            assert_eq!(fakes.argv("groupmod").unwrap(), ["-g", "5", "root"]);
            assert_eq!(moved["changed"], true);
            // Read back after the command, which here changed nothing.
            assert_eq!(moved["gid"], 0);

            let gone = answer(
                &args(json!({"name": "root", "state": "absent"})),
                &fakes.context,
                unbounded(),
            )
            .unwrap();
            assert_eq!(fakes.argv("groupdel").unwrap(), ["root"]);
            assert_eq!(gone["changed"], true);
            assert_eq!(gone["state"], "absent");
        }

        /// A command that fails is the module's failure: `name` and the error output as `msg`.
        ///
        /// What would make this red: a failure answered as a change, or with the output as `msg`.
        #[test]
        fn a_failing_groupadd_is_the_module_s_failure() {
            let fakes = Fakes::new("groupadd-fails", &["groupadd"], 9);
            let name = scratch_name("volantf");
            let answer = answer(&args(json!({"name": name})), &fakes.context, unbounded()).unwrap();
            assert_eq!(answer["failed"], true);
            assert_eq!(answer["msg"], "err\n");
            assert_eq!(answer["name"], name.as_str());
            assert!(!answer.contains_key("changed"));
        }

        /// Every argument outside `name`, `state`, `gid` and `system` hands back before any
        /// command runs.
        ///
        /// What would make this red: one of them answered, or a command run before handing back.
        #[test]
        fn an_argument_outside_the_subset_hands_back_before_any_command() {
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
                let answer = answer(&given, &fakes.context, unbounded());
                assert!(
                    matches!(answer, Err(Stop::HandBack(_))),
                    "{key} was answered"
                );
            }
            assert_eq!(fakes.argv("groupadd"), None);
        }

        /// A group `/etc/group` does not hold and the name service knows is handed back, before
        /// `groupadd` or `groupdel` runs.
        ///
        /// What would make this red: the name service not asked, which reads the group as
        /// missing and runs `groupadd` for a group the directory already has.
        #[test]
        fn a_group_only_the_name_service_knows_hands_back() {
            let fakes = Fakes::new("group-nss", &["groupadd", "groupdel"], 0);
            let name = scratch_name("volantn");
            fake_getent(
                &fakes.scratch,
                &format!("[ \"$2\" = {name} ] && echo '{name}:x:5000:' && exit 0; exit 2"),
            );
            for state in ["present", "absent"] {
                let answer = answer(
                    &args(json!({"name": name, "state": state})),
                    &fakes.context,
                    unbounded(),
                );
                assert!(
                    matches!(answer, Err(Stop::HandBack(_))),
                    "{state} was answered"
                );
            }
            assert_eq!(fakes.argv("groupadd"), None);
            assert_eq!(fakes.argv("groupdel"), None);
        }
    }
}
