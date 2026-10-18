// SPDX-License-Identifier: GPL-3.0-or-later

use std::collections::BTreeMap;
use std::io::{BufReader, Read, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

use anyhow::{Context, bail};
use serde_json::Value;
use volant_protocol::frame::{read_frame, write_frame};

use super::reach::module_path;
use super::types::{HELPER, ModuleFacts, Natives, Resolved, Union, payload_key};
use crate::runtime::resolve;

/// The interpreters to try, in the order they are tried: [`crate::runtime::resolve`]'s order.
///
/// An explicit `VOLANT_PYTHON` is the only candidate when it is set: falling back from it to
/// `python3` would answer a path that does not exist, or has no ansible-core, by quietly
/// running something else.
pub(crate) fn candidates(explicit: Option<&str>, virtual_env: Option<&str>) -> Vec<String> {
    resolve::current(explicit, virtual_env)
        .into_iter()
        .map(|candidate| candidate.python)
        .collect()
}

/// The sentence the pre-flight prints when no candidate has ansible-core, naming what was
/// tried and the one variable that changes the answer.
pub(super) fn refusal_for(tried: &str) -> String {
    format!(
        "no Python interpreter with ansible-core: tried {tried}. Install ansible-core for one \
         of them, or set VOLANT_PYTHON to an interpreter that has it"
    )
}

/// A helper process and the two pipes a request travels over.
pub struct PythonBuilder {
    pub(super) child: Child,
    /// `None` only once [`Drop`] has closed it.
    pub(super) stdin: Option<ChildStdin>,
    /// `None` only once an [`super::ActionHelper`] reads it on a thread of its own.
    pub(super) stdout: Option<BufReader<ChildStdout>>,
    /// The interpreter it runs under, as it was started.
    pub(super) python: String,
}

impl PythonBuilder {
    /// Starts the helper under the first interpreter that has ansible-core.
    ///
    /// `Err` when none has, carrying the sentence the pre-flight prints. The probe is the
    /// import itself rather than a version string: an interpreter that cannot import `ansible`
    /// cannot build a payload, whatever it reports.
    pub fn start() -> anyhow::Result<PythonBuilder> {
        PythonBuilder::start_installing(true)
    }

    /// [`PythonBuilder::start`], installing Volant's runtime when no interpreter has
    /// ansible-core only if `install` and the run's `[volant] runtime` allow it.
    pub(crate) fn start_installing(install: bool) -> anyhow::Result<PythonBuilder> {
        let explicit = std::env::var("VOLANT_PYTHON").ok();
        let virtual_env = std::env::var("VIRTUAL_ENV").ok();
        start_from(explicit.as_deref(), virtual_env.as_deref(), install)
    }
    pub(crate) fn under(python: &str) -> anyhow::Result<PythonBuilder> {
        PythonBuilder::under_with(python, &[])
    }

    /// [`PythonBuilder::under`] with variables set for the helper alone, which is how a test
    /// points ansible-core at a collection it wrote without touching this process's environment.
    pub(super) fn under_with(python: &str, env: &[(&str, &str)]) -> anyhow::Result<PythonBuilder> {
        let from_runtime = crate::runtime::collections::helper_env();
        let mut env = env.to_vec();
        if let Some(paths) = from_runtime.as_deref()
            && !env
                .iter()
                .any(|(name, _)| *name == "ANSIBLE_COLLECTIONS_PATH")
        {
            env.push(("ANSIBLE_COLLECTIONS_PATH", paths));
        }
        PythonBuilder::spawn(python, &["-c", HELPER], &env)
    }

    pub(crate) fn spawn(
        python: &str,
        args: &[&str],
        env: &[(&str, &str)],
    ) -> anyhow::Result<PythonBuilder> {
        let mut child = Command::new(python)
            .args(args)
            .envs(env.iter().copied())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .with_context(|| format!("starting the python helper under {python}"))?;
        let stdin = child.stdin.take().expect("stdin was piped");
        let stdout = BufReader::new(child.stdout.take().expect("stdout was piped"));
        Ok(PythonBuilder {
            child,
            stdin: Some(stdin),
            stdout: Some(stdout),
            python: python.to_string(),
        })
    }

    /// The union blob for every module named, the facts each of them needs, and the files it was
    /// built from when the helper could name them all.
    ///
    /// An error here fails the run: a union that was not built whole is never sent, so no host
    /// can receive a blob missing the `module_utils` one of its tasks imports.
    pub fn union(
        &mut self,
        modules: &[String],
    ) -> anyhow::Result<(Union, Option<crate::union_cache::Traced>)> {
        let stdin = self.stdin.as_mut().expect("stdin is open until drop");
        exchange(
            stdin,
            self.stdout.as_mut().expect("stdout is read here"),
            modules,
        )
    }

    /// Adds each directory's plugin directories to the helper's loaders, as ansible-playbook
    /// adds a playbook's and a role's.
    pub fn add_plugin_dirs(&mut self, dirs: &[std::path::PathBuf]) -> anyhow::Result<()> {
        let dirs: Vec<&str> = dirs.iter().filter_map(|dir| dir.to_str()).collect();
        let module_path = module_path();
        let module_path: Vec<&str> = module_path.iter().filter_map(|dir| dir.to_str()).collect();
        if !dirs.is_empty() || !module_path.is_empty() {
            let stdin = self.stdin.as_mut().expect("stdin is open until drop");
            ask(
                stdin,
                self.stdout.as_mut().expect("stdout is read here"),
                &serde_json::json!({ "plugin_dirs": dirs, "module_path": module_path }),
                "add the plugin directories",
            )?;
        }
        Ok(())
    }

    /// One frame out and the next one in, for a caller that frames its own requests. `Ok(None)`
    /// when the helper closed its output before answering.
    pub(crate) fn round_trip(&mut self, request: &[u8]) -> std::io::Result<Option<Vec<u8>>> {
        let stdin = self.stdin.as_mut().expect("stdin is open until drop");
        write_frame(stdin, request)?;
        read_frame(self.stdout.as_mut().expect("stdout is read here"))
    }

    /// Ends the helper and says how it went, for the message of a call that found it gone.
    pub(crate) fn end(&mut self) -> String {
        drop(self.stdin.take());
        let _ = self.child.kill();
        self.child
            .wait()
            .map_or_else(|err| err.to_string(), |status| status.to_string())
    }

    /// What the controller's ansible-core makes of each name, one answer per name asked.
    pub fn resolve(&mut self, modules: &[String]) -> anyhow::Result<BTreeMap<String, Resolved>> {
        let stdin = self.stdin.as_mut().expect("stdin is open until drop");
        resolve_exchange(
            stdin,
            self.stdout.as_mut().expect("stdout is read here"),
            modules,
        )
    }
}

/// How long a dropped helper gets to exit on its own before it is killed.
const HELPER_EXIT_GRACE: std::time::Duration = std::time::Duration::from_secs(2);

impl Drop for PythonBuilder {
    /// Asked to stop first: closing stdin ends the helper's loop, and the interpreter's exit
    /// handlers remove the `ansible-local-*` directory ansible-core made at import. A helper
    /// still running after `HELPER_EXIT_GRACE` (wedged in a build) is killed, and the wait is
    /// what keeps it from being left as a zombie.
    fn drop(&mut self) {
        drop(self.stdin.take());
        let deadline = std::time::Instant::now() + HELPER_EXIT_GRACE;
        while std::time::Instant::now() < deadline {
            match self.child.try_wait() {
                Ok(Some(_)) | Err(_) => return,
                Ok(None) => std::thread::sleep(std::time::Duration::from_millis(10)),
            }
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// The probing, split from the environment read so the order that is walked, and the refusal
/// when nothing is left, are both testable without touching the process environment.
pub(super) fn start_from(
    explicit: Option<&str>,
    virtual_env: Option<&str>,
    install: bool,
) -> anyhow::Result<PythonBuilder> {
    PythonBuilder::under(&find_python(explicit, virtual_env, install)?)
}

/// The first candidate interpreter that has ansible-core; else, when `install` and the mode allow
/// it, Volant's runtime installed now; else the refusal naming what was tried.
pub(super) fn find_python(
    explicit: Option<&str>,
    virtual_env: Option<&str>,
    install: bool,
) -> anyhow::Result<String> {
    let tried = candidates(explicit, virtual_env);
    let found = tried.iter().find(|python| resolve::has_ansible(python));
    if let Some(python) = found {
        return Ok(python.clone());
    }
    let mode = resolve::mode();
    if install && resolve::may_install(mode, explicit) {
        return resolve::install_now().map_err(anyhow::Error::msg);
    }
    if tried.is_empty() {
        bail!(
            "Volant's Python runtime is not installed and `[volant] runtime` is `managed`: run \
             `volant runtime install`"
        );
    }
    bail!(
        "{}{}",
        refusal_for(&tried.join(", ")),
        resolve::hint(mode, explicit.is_some())
    )
}

/// One request and its answer, over anything that reads and writes.
///
/// Split out from [`PythonBuilder::union`] so the two ways an answer goes wrong - the helper
/// dying without writing, and the helper reporting a module it could not build - are testable
/// without an ansible-core on the machine running the tests.
///
/// The files the helper reports building from, and the interpreter it ran under, come back beside
/// the union, `None` when it could not name them all, which keeps the union out of the cache.
pub(super) fn exchange<W: Write, R: Read>(
    to: W,
    from: R,
    modules: &[String],
) -> anyhow::Result<(Union, Option<crate::union_cache::Traced>)> {
    let answer = ask(
        to,
        from,
        &serde_json::json!({ "modules": modules }),
        "build the modules",
    )?;
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
        // Keyed by `payload_key` whatever the playbook wrote, so `ping` and
        // `ansible.legacy.ping` in one run are one entry, a collection's `sysctl` is never
        // another's, and `prepare` finds what was built for a task by the same function.
        facts.insert(payload_key(name).to_string(), module_facts(name, value)?);
    }
    // The answer has to cover the request. Without this the helper - a future one, or one that
    // failed halfway and answered anyway - can report success having built nothing, and every
    // host then receives a blob holding none of the modules its tasks name.
    for asked in modules {
        if !facts.contains_key(payload_key(asked)) {
            bail!("the python helper built no payload for '{asked}'");
        }
    }
    let sources = answer
        .get("sources")
        .and_then(crate::union_cache::sources_from)
        .zip(answer.get("interpreter").and_then(Value::as_str))
        .map(|(sources, interpreter)| crate::union_cache::Traced {
            interpreter: interpreter.into(),
            sources,
        });
    Ok((
        Union {
            hash,
            zip_b64,
            modules: facts,
            refused: BTreeMap::new(),
            natives: Natives::default(),
        },
        sources,
    ))
}

/// One request to the helper and its answer, refusing a dead helper, an answer that is not JSON
/// and the helper's own error, which names what it could not `doing`.
fn ask<W: Write, R: Read>(
    mut to: W,
    from: R,
    request: &Value,
    doing: &str,
) -> anyhow::Result<Value> {
    write_frame(&mut to, request.to_string().as_bytes())
        .context("sending a module request to the python helper")?;
    let Some(answer) = read_frame(from).context("reading the python helper's answer")? else {
        bail!("the python helper exited before answering; no module payload was built");
    };
    let answer: Value = serde_json::from_slice(&answer)
        .context("the python helper answered something that is not JSON")?;
    if let Some(error) = answer.get("error").and_then(Value::as_str) {
        bail!("the python helper could not {doing}: {error}");
    }
    Ok(answer)
}

/// The `resolve` request and its answer, one [`Resolved`] per name asked.
///
/// A name left unanswered, or answered in a shape this does not know, is refused rather than
/// read as missing: a module taken for missing is only dropped from the union when a role file
/// alone names it, and a helper that answered nothing would then build a run whose collection
/// modules all fail per host.
pub(super) fn resolve_exchange<W: Write, R: Read>(
    to: W,
    from: R,
    modules: &[String],
) -> anyhow::Result<BTreeMap<String, Resolved>> {
    let answer = ask(
        to,
        from,
        &serde_json::json!({ "resolve": modules }),
        "resolve the modules",
    )?;
    let answers = answer
        .get("resolved")
        .and_then(Value::as_object)
        .context("the python helper's answer carries no resolution")?;
    let mut out = BTreeMap::new();
    for asked in modules {
        let value = answers
            .get(asked)
            .with_context(|| format!("the python helper did not resolve '{asked}'"))?;
        out.insert(asked.clone(), resolved_from(asked, value)?);
    }
    Ok(out)
}

/// One name's answer, as the helper gives it and the union cache keeps it.
pub(crate) fn resolved_from(asked: &str, value: &Value) -> anyhow::Result<Resolved> {
    let text =
        |value: &Value, key: &str| value.get(key).and_then(Value::as_str).map(str::to_string);
    Ok(if let Some(fqcn) = text(value, "module") {
        let collection = match value.get("collection").unwrap_or(&Value::Null) {
            Value::Null => None,
            Value::Array(pair) => match pair.as_slice() {
                [Value::String(name), Value::String(version)] => {
                    Some((name.clone(), version.clone()))
                }
                _ => bail!(
                    "the python helper named the collection of '{asked}' as {pair:?}, not as a name and a version"
                ),
            },
            other => bail!(
                "the python helper named the collection of '{asked}' as {other}, not as a name and a version"
            ),
        };
        Resolved::Module { fqcn, collection }
    } else if let Some(fqcn) = text(value, "action_plugin") {
        Resolved::ActionPlugin {
            fqcn,
            with_module: value.get("with_module") == Some(&Value::Bool(true)),
        }
    } else if let Some(reason) = text(value, "unusable") {
        Resolved::Unusable { reason }
    } else if let Some(missing) = value.get("missing") {
        Resolved::Missing {
            collection: missing.as_str().map(str::to_string),
        }
    } else {
        bail!(
            "the python helper resolved '{asked}' to something this release cannot read: {value}"
        );
    })
}

/// [`resolved_from`] the other way.
pub(crate) fn resolved_json(resolved: &Resolved) -> Value {
    match resolved {
        Resolved::Module { fqcn, collection } => serde_json::json!({
            "module": fqcn,
            "collection": collection.as_ref().map(|(name, version)| [name, version]),
        }),
        Resolved::ActionPlugin { fqcn, with_module } => {
            serde_json::json!({ "action_plugin": fqcn, "with_module": with_module })
        }
        Resolved::Unusable { reason } => serde_json::json!({ "unusable": reason }),
        Resolved::Missing { collection } => serde_json::json!({ "missing": collection }),
    }
}

pub(crate) fn module_facts(name: &str, value: &Value) -> anyhow::Result<ModuleFacts> {
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
        core: value.get("core").and_then(Value::as_bool).unwrap_or(false),
    })
}
