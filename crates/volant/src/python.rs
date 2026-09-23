// SPDX-License-Identifier: GPL-3.0-or-later
//! The controller's Python helper: one interpreter, alive for the whole run, that builds the
//! module payloads a Python task needs.
//!
//! ansible-core caches a module's zip by the module's name, so the first build of a module
//! costs 140 ms and every later one 2.8 ms - measured on ansible-core 2.19.12. A helper started
//! per task would pay the cold price every time, which is why this one is started once and
//! kept.
//!
//! What it hands back is **one** union zip for every module the run names, not one zip per
//! module: the entries the per-module zips share are byte-identical, so merging them is well
//! defined, and the union of the five modules this release ships is 631 KB against the 2.3 MB
//! the five separate zips add up to. The helper refuses a merge conflict rather than letting
//! the last module written win, because a blob whose `module_utils` came from an arbitrary
//! module would run something the playbook did not ask for.

use std::collections::BTreeMap;
use std::io::{BufReader, Read, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

use anyhow::{Context, bail};
use serde_json::{Map, Value};
use volant_protocol::frame::{read_frame, write_frame};
use volant_protocol::modules::short_name;

/// The helper itself, shipped in the binary and handed to the interpreter on its command line.
/// Nothing is written to disk for it, so no temporary file can be left behind by a run that
/// dies.
const HELPER: &str = include_str!("python_helper.py");

/// The per-module facts a task needs alongside the shared blob, as the wrapper ansible-core
/// built for that module reports them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModuleFacts {
    /// `ansible.modules.ping`, the name the module is imported under.
    pub module_fqn: String,
    /// `legacy` on ansible-core 2.19.12; the serialisation profile the module was built for.
    pub profile: String,
    /// `rlimit_nofile` from the wrapper; 0 means leave the limit alone.
    pub rlimit_nofile: u64,
    /// The wrapper's `extensions`; empty on ansible-core 2.19.12.
    pub extensions: Map<String, Value>,
}

/// One union zip and the facts of every module merged into it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Union {
    /// Lowercase hex blake3 of the **decoded** zip, which is what names the blob on the wire
    /// and on the host. The agent hashes the bytes it decoded and refuses a blob whose name
    /// they do not match, so hashing anything else here - the base64 text is the easy mistake,
    /// being the same length of hex - leaves the two sides unable to agree on what a blob is
    /// called, and every run re-uploads a payload the host already holds.
    pub hash: String,
    /// The zip, base64 exactly as ansible-core produced its parts: the wrapper carries
    /// `zip_data` already encoded, so re-encoding raw bytes for the frame would decode and
    /// encode the same 631 KB for nothing.
    pub zip_b64: String,
    /// Keyed by the short module name, as `short_name` reads it, whatever spelling the playbook
    /// used: `ping` and `ansible.builtin.ping` are one entry.
    pub modules: BTreeMap<String, ModuleFacts>,
}

/// Whether this module runs through the warm Python path.
///
/// A module ansible-core ships that this release runs neither on the agent nor on the controller,
/// and that the reference does not run through an action plugin. The first two say there is no
/// native path for it; the third says sending the module alone would run something that is not
/// what the playbook asked for, which is why those twenty names stay refused rather than joining
/// this set.
///
/// **One definition, read by the pre-flight as well as by the driver.** The pre-flight arm that
/// lets a module reach a host calls this, so a name added or removed here cannot be admitted
/// before the first connection and then refused per host - a startup refusal silently turned
/// into a failure on every host by a change that looks local.
///
/// The five modules whose only product is facts - `setup`, `getent`, `package_facts`,
/// `service_facts` and `mount_facts` - were held out of this set while nothing merged a result's
/// `ansible_facts` into the variable store, because they would have reported `ok` and left the
/// next task reading `ansible_facts.*` undefined. `record_facts` merges them now, so they are in
/// it.
pub fn is_python_module(module: &str) -> bool {
    use volant_protocol::modules::{
        import_module, include_module, is_builtin, is_known, short_name,
    };

    is_builtin(module)
        && !is_known(module)
        // The statements and the pseudo-module this engine answers itself. They are builtin
        // module names and nothing ansible-core would build a payload for: an `include_tasks`
        // sent to a host as a payload would ask the agent to run the statement the compiler
        // exists to resolve, and `meta` asks the engine for something rather than the host.
        && import_module(module).is_none()
        && include_module(module).is_none()
        && short_name(module) != crate::playbook::META
        && !crate::action_plugins::is_action_backed(module)
        // A plugin this release runs is not a module either: it picks one, and the sub-tasks it
        // asks for carry the payloads. `package` is gone from the refused list, so without this
        // line it would read here as a module of its own.
        && crate::action_plugins::kind(module).is_none()
}

/// The short names of every module a run needs in its union, from the modules its steps and
/// handlers name: a Python module for itself, a plugin for every module it may run.
///
/// Every backend a plugin may pick goes in before the first connection, because nothing is built
/// once the facts are known. A plugin's own name never goes in: the reference never runs the
/// `package` module, its plugin is where the choice lives.
pub fn modules_to_build<'a>(
    modules: impl IntoIterator<Item = &'a str>,
) -> std::collections::BTreeSet<String> {
    let mut out = std::collections::BTreeSet::new();
    for module in modules {
        if let Some(kind) = crate::action_plugins::kind(module) {
            out.extend(
                crate::action_plugins::modules_for(kind)
                    .iter()
                    .map(|m| (*m).to_string()),
            );
        } else if is_python_module(module) {
            out.insert(short_name(module).to_string());
        }
    }
    out
}

/// One union blob for a whole run, or `None` when no task of it needs one.
///
/// Built once, before the first connection, under a helper that lives no longer than the build:
/// ansible-core caches a module's zip by name inside one process, which is what makes building
/// five modules in one call cost one cold build and four warm ones.
///
/// An error here is a refusal the operator reads instead of a run that starts, connects, and
/// then cannot run its first Python task.
pub fn union_for(modules: &std::collections::BTreeSet<String>) -> anyhow::Result<Option<Union>> {
    if modules.is_empty() {
        return Ok(None);
    }
    let names: Vec<String> = modules.iter().cloned().collect();
    PythonBuilder::start()
        .map_err(|err| no_builder(&err, &names))?
        .union(&names)
        .map(Some)
}

/// The sentence a run that gathers facts and nothing else gets on top of [`refusal_for`].
///
/// `gather_facts` is on unless a play turns it off, so this refusal is the first thing a playbook
/// of native tasks alone meets on a controller with no ansible-core - and the way out it wants is
/// not the one the refusal names. An operator who has never installed ansible-core, and who never
/// asked for a Python module, should not have to work out that the play keyword is what put one
/// in the run.
const GATHERING_ONLY: &str = "This run needs ansible-core only to gather facts, so writing \
                              `gather_facts: false` on the play is the other way out";

/// The refusal an operator reads when the controller cannot build payloads, widened with the way
/// out that exists only when gathering facts is the whole reason one was wanted.
///
/// Said only when it is true: a run naming `lineinfile` needs ansible-core whatever the play says
/// about facts, and pointing that operator at `gather_facts: false` sends them to try something
/// that cannot work.
fn no_builder(err: &anyhow::Error, modules: &[String]) -> anyhow::Error {
    match modules {
        [only] if only == "setup" => anyhow::anyhow!("{err:#}. {GATHERING_ONLY}"),
        _ => anyhow::anyhow!("{err:#}"),
    }
}

/// What a task needs of a payload before it knows which host it is going to.
///
/// A payload has two halves with two different lifetimes. This is the module's: the blob that
/// carries it and the facts ansible-core reported building it, all of them identical on every
/// host. The other half is the interpreter, which is the host's own and is not known until its
/// agent has said what it has.
///
/// They are kept apart rather than kept together and filled in later, so that a payload with no
/// interpreter in it cannot exist: the wire `PythonPayload` is only ever built where the chosen
/// interpreter is in hand, and a module run under an interpreter nobody chose is the failure
/// this whole path is built to avoid.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModulePayload {
    /// The union blob that holds this module, by its hash.
    pub blob: String,
    pub facts: ModuleFacts,
}

impl ModulePayload {
    /// The payload as the agent receives it, under the interpreter chosen for its host.
    pub fn under(&self, interpreter: &str) -> volant_protocol::PythonPayload {
        volant_protocol::PythonPayload {
            blob: self.blob.clone(),
            module_fqn: self.facts.module_fqn.clone(),
            profile: self.facts.profile.clone(),
            rlimit_nofile: self.facts.rlimit_nofile,
            extensions: self.facts.extensions.clone(),
            interpreter: interpreter.to_string(),
        }
    }
}

/// The interpreters to try, in the order they are tried.
///
/// An explicit `VOLANT_PYTHON` is the only candidate when it is set: falling back from it to
/// `python3` would answer a path that does not exist, or has no ansible-core, by quietly
/// running something else.
fn candidates(explicit: Option<&str>, virtual_env: Option<&str>) -> Vec<String> {
    if let Some(python) = explicit {
        return vec![python.to_string()];
    }
    let mut out = Vec::new();
    if let Some(env) = virtual_env {
        out.push(format!("{env}/bin/python"));
    }
    out.push("python3".to_string());
    out
}

/// The sentence the pre-flight prints when no candidate has ansible-core, naming what was
/// tried and the one variable that changes the answer.
fn refusal_for(tried: &str) -> String {
    format!(
        "no Python interpreter with ansible-core: tried {tried}. Install ansible-core for one \
         of them, or set VOLANT_PYTHON to an interpreter that has it"
    )
}

/// A helper process and the two pipes a request travels over.
pub struct PythonBuilder {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
}

impl PythonBuilder {
    /// Starts the helper under the first interpreter that has ansible-core.
    ///
    /// `Err` when none has, carrying the sentence the pre-flight prints. The probe is the
    /// import itself rather than a version string: an interpreter that cannot import `ansible`
    /// cannot build a payload, whatever it reports.
    pub fn start() -> anyhow::Result<PythonBuilder> {
        let explicit = std::env::var("VOLANT_PYTHON").ok();
        let virtual_env = std::env::var("VIRTUAL_ENV").ok();
        start_from(explicit.as_deref(), virtual_env.as_deref())
    }
    fn under(python: &str) -> anyhow::Result<PythonBuilder> {
        let mut child = Command::new(python)
            .args(["-c", HELPER])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .with_context(|| format!("starting the python helper under {python}"))?;
        let stdin = child.stdin.take().expect("stdin was piped");
        let stdout = BufReader::new(child.stdout.take().expect("stdout was piped"));
        Ok(PythonBuilder {
            child,
            stdin,
            stdout,
        })
    }

    /// The union blob for every module named, and the facts each of them needs.
    ///
    /// An error here fails the run: a union that was not built whole is never sent, so no host
    /// can receive a blob missing the `module_utils` one of its tasks imports.
    pub fn union(&mut self, modules: &[String]) -> anyhow::Result<Union> {
        exchange(&mut self.stdin, &mut self.stdout, modules)
    }
}

impl Drop for PythonBuilder {
    /// Killed rather than asked to stop: the helper holds nothing a run needs once the last
    /// answer is in, and an interpreter wedged in a build would otherwise outlive the run that
    /// started it. The wait is what keeps it from being left as a zombie.
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// The probing, split from the environment read so the order that is walked, and the refusal
/// when nothing is left, are both testable without touching the process environment.
fn start_from(explicit: Option<&str>, virtual_env: Option<&str>) -> anyhow::Result<PythonBuilder> {
    let tried = candidates(explicit, virtual_env);
    let found = tried.iter().find(|python| {
        Command::new(python)
            .args(["-c", "import ansible"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
    });
    let Some(python) = found else {
        bail!(refusal_for(&tried.join(", ")));
    };
    PythonBuilder::under(python)
}

/// One request and its answer, over anything that reads and writes.
///
/// Split out from [`PythonBuilder::union`] so the two ways an answer goes wrong - the helper
/// dying without writing, and the helper reporting a module it could not build - are testable
/// without an ansible-core on the machine running the tests.
fn exchange<W: Write, R: Read>(mut to: W, from: R, modules: &[String]) -> anyhow::Result<Union> {
    let request = serde_json::json!({ "modules": modules });
    write_frame(&mut to, request.to_string().as_bytes())
        .context("sending a module request to the python helper")?;
    let Some(answer) = read_frame(from).context("reading the python helper's answer")? else {
        bail!("the python helper exited before answering; no module payload was built");
    };
    let answer: Value = serde_json::from_slice(&answer)
        .context("the python helper answered something that is not JSON")?;
    if let Some(error) = answer.get("error").and_then(Value::as_str) {
        bail!("the python helper could not build the modules: {error}");
    }
    let zip_b64 = answer
        .get("zip_b64")
        .and_then(Value::as_str)
        .context("the python helper's answer carries no zip")?
        .to_string();
    if zip_b64.is_empty() {
        bail!("the python helper answered an empty blob; no module payload was built");
    }
    // Hashed here rather than where it is sent, so the name and the bytes are settled in one
    // place and by one decode.
    // The agent names the blob by what the same decoder reads, so the two cannot disagree on it.
    let zip = volant_protocol::encoding::b64_decode(&zip_b64)
        .map_err(|err| anyhow::anyhow!("the python helper's blob is not base64: {err}"))?;
    let hash = blake3::hash(&zip).to_hex().to_string();
    let mut facts = BTreeMap::new();
    let built = answer
        .get("modules")
        .and_then(Value::as_object)
        .context("the python helper's answer carries no module facts")?;
    for (name, value) in built {
        // Keyed by the short name whatever the playbook wrote, so `ping` and
        // `ansible.builtin.ping` in one run are one entry and a caller that reads its task's
        // module the way the registries read it finds what was built for it.
        facts.insert(short_name(name).to_string(), module_facts(name, value)?);
    }
    // The answer has to cover the request. Without this the helper - a future one, or one that
    // failed halfway and answered anyway - can report success having built nothing, and every
    // host then receives a blob holding none of the modules its tasks name.
    for asked in modules {
        if !facts.contains_key(short_name(asked)) {
            bail!("the python helper built no payload for '{asked}'");
        }
    }
    Ok(Union {
        hash,
        zip_b64,
        modules: facts,
    })
}

fn module_facts(name: &str, value: &Value) -> anyhow::Result<ModuleFacts> {
    let field = |key: &str| {
        value
            .get(key)
            .and_then(Value::as_str)
            .map(str::to_string)
            .with_context(|| format!("the python helper reported no {key} for '{name}'"))
    };
    Ok(ModuleFacts {
        module_fqn: field("module_fqn")?,
        profile: field("profile")?,
        rlimit_nofile: value
            .get("rlimit_nofile")
            .and_then(Value::as_u64)
            .with_context(|| format!("the python helper reported no rlimit_nofile for '{name}'"))?,
        extensions: value
            .get("extensions")
            .and_then(Value::as_object)
            .cloned()
            .with_context(|| format!("the python helper reported no extensions for '{name}'"))?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    /// The controller's interpreter is chosen in one order and that order is visible: an
    /// explicit `VOLANT_PYTHON`, then the active virtualenv, then `python3` on `PATH`.
    ///
    /// What would make this red: `PATH` consulted first, which would pick the system Python on
    /// a machine whose ansible-core lives in a virtualenv - measured, the dev host is exactly
    /// that machine, and its `/usr/bin/python3` has no ansible-core.
    #[test]
    fn the_controller_interpreter_follows_one_visible_order() {
        assert_eq!(candidates(Some("/opt/py"), Some("/venv"))[0], "/opt/py");
        assert_eq!(candidates(None, Some("/venv"))[0], "/venv/bin/python");
        assert_eq!(candidates(None, None), vec!["python3".to_string()]);
    }

    /// An interpreter without ansible-core is refused before the first connection, in words
    /// that say what to install and which interpreter was tried.
    #[test]
    fn an_interpreter_without_ansible_core_is_refused_before_connecting() {
        let err = refusal_for("/usr/bin/python3");
        assert!(err.contains("/usr/bin/python3"), "{err}");
        assert!(err.contains("ansible-core"), "{err}");
        assert!(err.contains("VOLANT_PYTHON"), "{err}");
    }

    /// A run that wanted ansible-core only to gather facts is told the way out that costs
    /// nothing, and a run that wanted it for a module the playbook named is not.
    ///
    /// What would make this red: the sentence left as it was. `gather_facts` is on unless a play
    /// turns it off, so this refusal is what a playbook of native tasks alone now meets on a
    /// controller with no ansible-core, and "install ansible-core" is the expensive half of the
    /// answer. Red the other way if the hint is unconditional: an operator whose playbook names
    /// `lineinfile` would be sent to write `gather_facts: false` and find the run refused all the
    /// same.
    #[test]
    fn a_run_that_only_gathers_is_told_it_can_stop_gathering() {
        let base = anyhow::anyhow!("{}", refusal_for("python3"));
        let only = format!("{:#}", no_builder(&base, &["setup".to_string()]));
        assert!(only.contains("gather_facts: false"), "{only}");
        assert!(only.contains("VOLANT_PYTHON"), "{only}");
        let also = format!(
            "{:#}",
            no_builder(&base, &["lineinfile".to_string(), "setup".to_string()])
        );
        assert!(!also.contains("gather_facts"), "{also}");
        assert!(also.contains("ansible-core"), "{also}");
    }

    /// `VOLANT_PYTHON` is the only candidate when it is set, so a path that does not exist is
    /// refused by its own name.
    ///
    /// What would make this red: appending the usual candidates behind it, which would build
    /// the payloads under a different interpreter than the operator named and say nothing.
    #[test]
    fn an_explicit_interpreter_is_never_fallen_back_from() {
        assert_eq!(
            candidates(Some("/nowhere/python"), Some("/venv")),
            vec!["/nowhere/python".to_string()]
        );
        let err = start_from(Some("/nowhere/python"), None)
            .map(|_| ())
            .expect_err("an interpreter that does not exist cannot build a payload")
            .to_string();
        assert!(err.contains("/nowhere/python"), "{err}");
        assert!(err.contains("VOLANT_PYTHON"), "{err}");
    }

    /// The probing itself walks the order and refuses when nothing is left, which is what the
    /// development machine exercises: its `python3` has no ansible-core.
    ///
    /// What would make this red: a candidate accepted because it merely ran - the probe is the
    /// `import ansible` exiting 0, not the interpreter existing - or a refusal that names
    /// something other than what was tried.
    #[test]
    fn nothing_is_started_under_an_interpreter_without_ansible_core() {
        let err = start_from(Some("/nowhere/python"), Some("/venv"))
            .map(|_| ())
            .expect_err("no candidate here has ansible-core")
            .to_string();
        assert!(
            err.contains("no Python interpreter with ansible-core"),
            "{err}"
        );
        assert!(
            !err.contains("/venv"),
            "an explicit interpreter stands alone: {err}"
        );
    }

    /// A helper that dies without writing fails the run. What would make this red: reading a
    /// closed pipe as an empty union, which would ship a blob holding no `module_utils` at all
    /// and fail every task on the host instead of the run on the controller.
    #[test]
    fn a_helper_that_dies_without_answering_fails_the_run() {
        let err = exchange(Vec::new(), Cursor::new(Vec::new()), &["ping".to_string()])
            .unwrap_err()
            .to_string();
        assert!(err.contains("exited before answering"), "{err}");
    }

    /// What the helper could not build comes back naming the module. What would make this red:
    /// dropping the helper's own sentence, which is the only place the module's name appears.
    #[test]
    fn a_module_the_helper_cannot_build_comes_back_named() {
        let answer = br#"{"error": "RuntimeError: ansible-core has no module 'nosuch'"}"#;
        let err = exchange(Vec::new(), framed(answer), &["nosuch".to_string()])
            .unwrap_err()
            .to_string();
        assert!(err.contains("nosuch"), "{err}");
    }

    /// The request names the modules asked for, and the facts come back keyed by module.
    #[test]
    fn a_union_carries_the_blob_and_one_set_of_facts_per_module() {
        let answer = br#"{"zip_b64": "UEsD", "modules": {"ping": {"module_fqn":
            "ansible.modules.ping", "profile": "legacy", "rlimit_nofile": 0,
            "extensions": {}}}}"#;
        let mut sent = Vec::new();
        let union = exchange(&mut sent, framed(answer), &["ping".to_string()]).unwrap();
        assert!(
            String::from_utf8_lossy(&sent).contains(r#"{"modules":["ping"]}"#),
            "{sent:?}"
        );
        assert_eq!(union.zip_b64, "UEsD");
        assert_eq!(
            union.modules["ping"],
            ModuleFacts {
                module_fqn: "ansible.modules.ping".to_string(),
                profile: "legacy".to_string(),
                rlimit_nofile: 0,
                extensions: Map::new(),
            }
        );
    }

    /// Every module asked for has to come back. What would make this red: reading the answer's
    /// module map on its own, which answers five modules asked for and none built with an empty
    /// union and `Ok` - a run that then sends every host a blob holding nothing it needs.
    #[test]
    fn an_answer_that_leaves_a_module_out_is_refused() {
        let asked = ["ping", "stat", "file", "copy", "setup"].map(str::to_string);
        let answer = br#"{"zip_b64": "UEsD", "modules": {}}"#;
        let err = exchange(Vec::new(), framed(answer), &asked)
            .unwrap_err()
            .to_string();
        assert!(err.contains("ping"), "{err}");
    }

    /// The facts are keyed by the short module name whatever spelling the playbook used, so one
    /// module asked for under two names is one entry.
    ///
    /// What would make this red: keying by the request string, which hands a map keyed
    /// `ansible.builtin.ping` to a caller that looks its task's module up by its short name and
    /// finds nothing - or, worse, an empty fact set.
    #[test]
    fn the_facts_are_keyed_by_the_short_module_name() {
        let answer = br#"{"zip_b64": "UEsD", "modules": {"ansible.builtin.ping": {"module_fqn":
            "ansible.modules.ping", "profile": "legacy", "rlimit_nofile": 0,
            "extensions": {}}}}"#;
        let asked = ["ansible.builtin.ping".to_string()];
        let union = exchange(Vec::new(), framed(answer), &asked).unwrap();
        assert_eq!(union.modules.keys().collect::<Vec<_>>(), ["ping"]);
    }

    /// An empty blob is refused on the controller. What would make this red: letting it through,
    /// which moves the refusal from before the first connection to every host in the run.
    #[test]
    fn an_empty_blob_is_refused_before_it_is_sent() {
        let answer = br#"{"zip_b64": "", "modules": {}}"#;
        let err = exchange(Vec::new(), framed(answer), &[])
            .unwrap_err()
            .to_string();
        assert!(err.contains("empty"), "{err}");
    }

    /// Which modules go through the payload path: what ansible-core ships, minus what this
    /// release runs itself, minus what the reference runs through an action plugin.
    ///
    /// What would make this red: the action-plugin names let in, which would send `package` or
    /// `template` as a module and run something the playbook did not ask for - the plugin is
    /// where their behaviour lives. Or a name this release already runs let in, which would ship
    /// a payload for `command` and stop running it natively.
    #[test]
    fn the_payload_path_takes_what_nothing_else_runs() {
        for through in ["lineinfile", "stat", "ping", "apt", "ansible.builtin.file"] {
            assert!(is_python_module(through), "{through}");
        }
        for native in [
            "command",
            "shell",
            "raw",
            "debug",
            "set_fact",
            "include_vars",
        ] {
            assert!(!is_python_module(native), "{native} runs here already");
        }
        for plugin in ["package", "service", "template", "copy", "assert"] {
            assert!(
                !is_python_module(plugin),
                "{plugin} needs its action plugin"
            );
        }
        assert!(
            !is_python_module("community.general.lineinfile"),
            "another collection's module is not ours to build"
        );
    }

    /// A run naming a plugin builds every module the plugin may pick, and never the plugin's own
    /// name.
    ///
    /// What would make this red: `package` in the set, which is `package` built and sent as
    /// itself - the reference never runs that module, its plugin is where the choice lives - or a
    /// backend missing, which leaves a host whose manager is `dnf` failing on a module the union
    /// was never asked for.
    #[test]
    fn a_plugin_brings_its_backends_into_the_union_and_not_itself() {
        let built = modules_to_build(["ansible.builtin.package", "service", "ping", "command"]);
        let built: Vec<&str> = built.iter().map(String::as_str).collect();
        assert_eq!(
            built,
            [
                "apt",
                "dnf",
                "dnf5",
                "ping",
                "service",
                "setup",
                "systemd",
                "systemd_service",
                "sysvinit"
            ]
        );
        assert!(!built.contains(&"package"), "{built:?}");
    }

    /// A run that names no Python module builds nothing, so a controller without ansible-core
    /// runs native playbooks exactly as before.
    ///
    /// What would make this red: the helper started for every run, which would refuse every
    /// playbook on a machine that has no ansible-core - including the ones that never needed it.
    #[test]
    fn a_run_with_no_python_module_builds_nothing() {
        let none = union_for(&std::collections::BTreeSet::new()).expect("nothing to build");
        assert!(none.is_none());
    }

    /// The blob is named by the hash of the zip's own bytes, which is the name the agent
    /// computes for the same blob when it decides whether it already holds it.
    ///
    /// What would make this red: hashing the base64 text instead of what it encodes. Both are
    /// 64 hex characters and both look like a working cache key, and the two sides would then
    /// never agree on what a blob is called: every run would upload a payload the agent already
    /// has, and no `has_blob` would ever answer yes. The vector is the agent's own,
    /// `UEsDBA==` for `PK\x03\x04`.
    #[test]
    fn the_blob_is_named_by_the_hash_of_the_zip_not_of_its_base64() {
        let answer = br#"{"zip_b64": "UEsDBA==", "modules": {"ping": {"module_fqn":
            "ansible.modules.ping", "profile": "legacy", "rlimit_nofile": 0,
            "extensions": {}}}}"#;
        let union = exchange(Vec::new(), framed(answer), &["ping".to_string()]).unwrap();
        assert_eq!(union.hash, blake3::hash(b"PK\x03\x04").to_hex().to_string());
        assert_ne!(union.hash, blake3::hash(b"UEsDBA==").to_hex().to_string());
        // The shape the agent refuses a name by before it will look for a file under it.
        assert_eq!(union.hash.len(), 64, "{}", union.hash);
        assert!(
            union
                .hash
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
            "{}",
            union.hash
        );
    }

    /// A blob that is not base64 is refused on the controller, naming where it broke.
    ///
    /// What would make this red: decoding what decodes and dropping the rest, which names the
    /// blob by the hash of bytes that are not the zip the agent will be sent.
    #[test]
    fn a_blob_that_is_not_base64_is_refused() {
        let answer = br#"{"zip_b64": "UEsD!BA==", "modules": {}}"#;
        let err = exchange(Vec::new(), framed(answer), &[])
            .unwrap_err()
            .to_string();
        assert!(err.contains("base64"), "{err}");
    }

    /// A truncated set of facts is an error rather than a default. What would make this red:
    /// filling `profile`, `rlimit_nofile` or `extensions` with a guess, which would run a module
    /// under a serialisation profile, or without an extension, it was not built for.
    #[test]
    fn missing_facts_are_refused_rather_than_guessed() {
        let answer = br#"{"zip_b64": "UEsD", "modules": {"ping": {"module_fqn":
            "ansible.modules.ping", "profile": "legacy", "rlimit_nofile": 0}}}"#;
        let err = exchange(Vec::new(), framed(answer), &["ping".to_string()])
            .unwrap_err()
            .to_string();
        assert!(err.contains("extensions"), "{err}");
        assert!(err.contains("ping"), "{err}");
        let answer = br#"{"zip_b64": "UEsD", "modules": {"ping": {"profile": "legacy"}}}"#;
        let err = exchange(Vec::new(), framed(answer), &["ping".to_string()])
            .unwrap_err()
            .to_string();
        assert!(err.contains("module_fqn"), "{err}");
        assert!(err.contains("ping"), "{err}");
    }

    /// The helper's own checks, run under whatever `python3` the machine has: no ansible-core
    /// is needed for the merge and the wrapper reading, which is where the union blob is made
    /// or silently spoiled.
    ///
    /// What would make this red: a merge that lets the last module written win over a
    /// conflicting `module_utils` entry, or a wrapper whose fields moved without this noticing.
    #[test]
    fn the_helper_refuses_a_byte_conflict_in_the_union() {
        let out = Command::new("python3")
            .args(["-c", HELPER, "--self-check"])
            .output()
            .expect("python3 is on PATH wherever these tests run");
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    /// A request and its answer over the helper's own pipes, which is the only place the two
    /// sides of the frame header meet.
    ///
    /// What would make this red: either end writing the length in the other's byte order, or
    /// the helper printing anything at all on stdout beside its frame - both would be invisible
    /// to the tests that feed a hand-built frame in. The machine deciding which branch runs is
    /// whether its `python3` has ansible-core; measured, the dev host's has not.
    #[test]
    fn the_helper_answers_one_frame_over_its_own_pipes() {
        let mut builder = PythonBuilder::under("python3").expect("python3 is on PATH");
        match builder.union(&["ping".to_string()]) {
            Ok(union) => assert!(!union.zip_b64.is_empty(), "an empty blob was accepted"),
            Err(refused) => assert!(
                refused.to_string().contains("could not build the modules"),
                "{refused}"
            ),
        }
    }

    fn framed(payload: &[u8]) -> Cursor<Vec<u8>> {
        let mut buf = Vec::new();
        write_frame(&mut buf, payload).unwrap();
        Cursor::new(buf)
    }
}
