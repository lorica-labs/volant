// SPDX-License-Identifier: GPL-3.0-or-later
//! Playbook loading: plays and tasks, with the keywords this release supports.

use std::path::{Path, PathBuf};

use anyhow::{Context, anyhow, bail};
use saphyr::{Scalar, Yaml};
use serde_json::{Map, Value};
use volant_protocol::modules::{
    import_module, include_module, is_known, local, native, short_name,
};

use crate::keywords::{
    BLOCK_SECTIONS, Support, block_keyword, handler_keyword, loop_control_keyword, play_keyword,
    task_keyword,
};
use crate::roles::{RoleEntry, RoleFrom};
use crate::yaml::{as_bool, field, to_json};

#[derive(Debug, Default)]
pub struct Playbook {
    pub plays: Vec<Play>,
}

#[derive(Debug, Clone)]
pub struct Play {
    pub name: String,
    pub hosts: String,
    /// The `hosts` value entry by entry, as it was written: one element for the string form,
    /// one per item for the list form. `--list-hosts` prints it as the Python list the
    /// reference prints, and `hosts: a,b` there is one pattern while `hosts: [a, b]` is two.
    pub host_patterns: Vec<String>,
    /// The play's own tags. Every task under it inherits them for the selection; the listing
    /// prints them on the play's own line.
    pub tags: Vec<String>,
    pub gather_facts: bool,
    /// Whether the play wrote `gather_facts` at all: `gathering = explicit` gathers only for a
    /// play that did.
    pub gather_written: bool,
    pub vars: Map<String, Value>,
    pub vars_files: Vec<String>,
    pub tasks: Vec<TaskOrBlock>,
    pub pre_tasks: Vec<TaskOrBlock>,
    pub post_tasks: Vec<TaskOrBlock>,
    pub roles: Vec<RoleEntry>,
    /// The play's own `handlers:` list, in the order it was written, which is the order they run
    /// in. A role's handlers join them at compile time, in front of these.
    pub handlers: Vec<HandlerTask>,
    /// `force_handlers`, unset when the play says nothing: `[defaults] force_handlers` and
    /// `--force-handlers` then speak instead, and the flag and the file are equivalent - measured
    /// on ansible-core 2.19.12, the same playbook exits with the same recap either way.
    pub force_handlers: Option<bool>,
    /// The directory of the file this play was written in, which is not always the directory of
    /// the playbook the operator named: `import_playbook` splices another file's plays in place,
    /// and everything a play reads from disk - its `vars_files`, its `group_vars/`, its roles -
    /// is looked for beside the file it came from.
    pub dir: PathBuf,
    /// Where the play was written, which `task path:` names for the `Gathering Facts` task.
    pub origin: Option<Origin>,
    /// `become`, unset when the play says nothing: a task and the host variables both get to
    /// speak before the connection defaults do.
    pub r#become: Option<bool>,
    pub become_user: Option<String>,
    /// `remote_user`, `port` and `connection`, folded into every task at compile time.
    pub connect: Connect,
    /// The execution strategy the play asked for, as written. The pre-flight refuses a name the
    /// engine has no strategy of.
    pub strategy: Option<String>,
    /// `order`, as written: the order the play's hosts run in. Checked when the hosts are
    /// resolved, where the reference checks it.
    pub order: Option<String>,
    /// The play's `timeout`, `throttle` and `debugger`: task keywords a play may carry, folded
    /// into every task as the outermost layer like `no_log`.
    pub timeout: Option<u64>,
    pub throttle: Option<u64>,
    pub debugger: Option<Debugger>,
    /// `serial`, as written: a number, a percentage string, a list of either, or a template over
    /// the play's variables. Kept raw because it is rendered when the play starts, which is where
    /// the reference renders it - see `compile::batches`.
    pub serial: Option<Value>,
    /// The play's own `no_log`, `environment`, `check_mode`, `diff` and `run_once`. All five are task
    /// keywords a play may also carry, so they are folded into every task at compile time rather
    /// than read again per step - see `compile`, which uses them as the outermost layer of the
    /// merge. `delegate_to` and `delegate_facts` are not here because `Play.fattributes` has
    /// neither: the reference takes them on a block and on a task and nowhere else.
    pub no_log: Option<bool>,
    pub environment: Vec<Value>,
    pub check_mode: Option<Flag>,
    pub diff: Option<Flag>,
    pub run_once: Option<bool>,
    /// The play's `module_defaults` and `collections`, folded into every task the same way. A
    /// role is the exception for `collections`: it does not inherit the play's.
    pub module_defaults: Vec<Value>,
    pub collections: Option<Vec<String>>,
    /// The play's `any_errors_fatal`, `ignore_errors` and `ignore_unreachable`: task keywords a
    /// play may carry, folded into every task as the outermost layer like `no_log` above.
    pub any_errors_fatal: Option<bool>,
    pub ignore_errors: Option<Flag>,
    pub ignore_unreachable: Option<bool>,
    /// `max_fail_percentage`, as written: a number, or a template rendered when the play
    /// starts. Play-only - a task or a block carrying it is refused at load.
    pub max_fail_percentage: Option<Value>,
    /// `gather_subset`, `gather_timeout` and `fact_path`, as written: the gathering step takes
    /// them as the arguments of its `setup`, and the play start checks their types.
    pub gather_subset: Option<Value>,
    pub gather_timeout: Option<Value>,
    pub fact_path: Option<Value>,
    /// `vars_prompt`, entry by entry as written, every key already known to the reference's
    /// list. Templated and asked when the play starts, see `prompt`.
    pub vars_prompt: Vec<Map<String, Value>>,
    /// Keywords the reference accepts and this release does not execute yet, sorted. The
    /// loader keeps the play rather than refusing it, so a playbook parses and lists the same
    /// way it does in the reference; the pre-flight then refuses the run before the first
    /// connection. Nothing may quietly drop a keyword between the two.
    pub unsupported: Vec<&'static str>,
}

/// One handler as the playbook wrote it: a task, plus the names it answers to besides its own.
///
/// `listen` is the one attribute `Handler.fattributes` has and `Task.fattributes` does not, which
/// is why a handler is read by its own function rather than by the ordinary task reader alone.
#[derive(Debug, Clone)]
pub struct HandlerTask {
    pub task: PlayTask,
    pub listen: Vec<String>,
}

/// One entry of a task list: a task, or a block grouping more of them.
#[derive(Debug, Clone)]
pub enum TaskOrBlock {
    Task(PlayTask),
    Block(Block),
}

/// A block: three task lists and the keywords its tasks inherit.
///
/// `keywords` is a [`PlayTask`] with an empty `module`, because a block carries a subset of the
/// task keywords and reading them twice is how the two would drift apart. Nothing runs it; the
/// compiler merges it into every task underneath.
#[derive(Debug, Clone)]
pub struct Block {
    pub body: Vec<TaskOrBlock>,
    pub rescue: Vec<TaskOrBlock>,
    pub always: Vec<TaskOrBlock>,
    pub keywords: PlayTask,
}

/// The task with nothing written on it, which is the outermost layer of every merge. It is
/// `PlayTask::empty` and not a derived zero: `loop_var` defaults to `item` and a derived
/// `Default` would leave it empty, which is a loop whose variable answers to no name.
/// Where a task was written: its file and the line and column its mapping starts at, both counted
/// from 1, as ansible-core's `Origin` names a task in a failure's error block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Origin {
    pub path: std::sync::Arc<str>,
    pub line: usize,
    pub col: usize,
}

thread_local! {
    /// The file being parsed and where its mappings start, while one is.
    static ORIGINS: std::cell::RefCell<Option<(std::sync::Arc<str>, crate::yaml::Origins)>> =
        const { std::cell::RefCell::new(None) };
}

/// Keeps a file's origins readable by [`task_origin`] until dropped, then puts back those of
/// the file that was being parsed before, if any: an import parses inside its importer.
struct OriginScope(Option<(std::sync::Arc<str>, crate::yaml::Origins)>);

impl OriginScope {
    fn enter(source: &str, origins: crate::yaml::Origins) -> Self {
        let path = std::path::absolute(source)
            .map_or_else(|_| source.to_string(), |p| p.display().to_string());
        Self(ORIGINS.with(|o| o.replace(Some((path.into(), origins)))))
    }
}

impl Drop for OriginScope {
    fn drop(&mut self) {
        let previous = self.0.take();
        ORIGINS.with(|o| *o.borrow_mut() = previous);
    }
}

/// Where `value` was written, when it is a mapping entry's value in the file being parsed.
fn value_origin(value: &Yaml<'_>) -> Option<Origin> {
    ORIGINS.with(|o| {
        o.borrow().as_ref().and_then(|(path, origins)| {
            origins.of_value(value).map(|(line, col)| Origin {
                path: path.clone(),
                line,
                col,
            })
        })
    })
}

/// Where the key of `value`'s entry was written, when it is a mapping entry's value in the file
/// being parsed.
fn key_origin(value: &Yaml<'_>) -> Option<Origin> {
    ORIGINS.with(|o| {
        o.borrow().as_ref().and_then(|(path, origins)| {
            origins.of_key(value).map(|(line, col)| Origin {
                path: path.clone(),
                line,
                col,
            })
        })
    })
}

/// Where `map` was written, when it comes from the file being parsed.
fn task_origin(map: &saphyr::Mapping<'_>) -> Option<Origin> {
    ORIGINS.with(|o| {
        o.borrow().as_ref().and_then(|(path, origins)| {
            origins.of(map).map(|(line, col)| Origin {
                path: path.clone(),
                line,
                col,
            })
        })
    })
}

impl Default for PlayTask {
    fn default() -> Self {
        PlayTask::empty()
    }
}

#[derive(Debug, Clone)]
pub struct PlayTask {
    pub name: String,
    /// Whether `name` was written on the task rather than filled in from the module. The two
    /// listings that name a task tell them apart: measured on ansible-core 2.19.12, a named
    /// task of a role lists as `role : name` while an unnamed one lists as its module alone,
    /// with no role prefix - and the banner prefixes both.
    pub named: bool,
    pub module: String,
    pub args: Map<String, Value>,
    /// The tags written here and on everything above it, sorted and free of duplicates. An
    /// empty list is what the reference calls `untagged`.
    pub tags: Vec<String>,
    /// Unset when neither the task nor the block above it said anything. It has to be a
    /// three-state value: measured on ansible-core 2.19.12, a block with `ignore_errors: true`
    /// and a task with `ignore_errors: false` inside it fails the run, so the task's own `false`
    /// has to be told apart from its silence.
    pub ignore_errors: Option<Flag>,
    pub timeout: Option<u64>,
    /// `async`: seconds the task may run as a background job, 0 for a task that is not one.
    pub async_val: u64,
    /// `poll`: seconds between two looks at the job, 0 for a task that only starts it. Unset is
    /// the reference's default of 15.
    pub poll: Option<u64>,
    /// `throttle`: the most hosts that run this task at the same time. Zero is no limit.
    pub throttle: Option<u64>,
    /// `debugger`: when a result of this task opens the debugger prompt.
    pub debugger: Option<Debugger>,
    pub vars: Map<String, Value>,
    /// Conditions that must all hold, as Jinja2 expressions.
    pub when: Vec<String>,
    /// Raw `loop` value: a list, or a template string rendering to one. `with_items` lands
    /// here too and is flattened one level at run time.
    pub loop_items: Option<Value>,
    /// Whether `loop_items` came from `with_items` rather than `loop`: `with_items` flattens
    /// one level, `loop` does not.
    pub with_items: bool,
    /// The lookup a `with_<lookup>` loop runs its terms through, when `loop_items` came from one
    /// other than `with_items`: the loop walks what that lookup returns as a list, the way the
    /// reference's `loop_with` does.
    pub loop_with: Option<String>,
    pub loop_var: String,
    pub loop_label: Option<String>,
    /// `loop_control.index_var`: the name each item's 0-based position is bound to.
    pub index_var: Option<String>,
    /// `loop_control.extended`, `extended_allitems` and `pause`, raw: the reference templates
    /// and converts them per task, so a bad value fails the task, not the load.
    pub loop_extended: Option<Value>,
    pub loop_allitems: Option<Value>,
    pub loop_pause: Option<Value>,
    /// `loop_control.break_when`: the loop ends after the first item for which all hold.
    pub break_when: Vec<String>,
    pub register: Option<String>,
    pub changed_when: Vec<String>,
    pub failed_when: Vec<String>,
    pub r#become: Option<bool>,
    pub become_user: Option<String>,
    /// `remote_user`, `port` and `connection`, the task's own over its block's over the play's.
    pub connect: Connect,
    /// The handlers this task asks for when it changes something, as written. A block's `notify`
    /// comes down here too, which is what makes a `notify` on a block apply to its tasks -
    /// measured on ansible-core 2.19.12.
    pub notify: Vec<String>,
    /// `until`: the conditions that end the retry loop, all of which must hold. A list is
    /// accepted the way `when` accepts one.
    pub until: Vec<String>,
    /// `retries` and `delay`, raw: both are templated against the task's own variables, so they
    /// are kept as written and rendered when the task runs.
    pub retries: Option<Value>,
    pub delay: Option<Value>,
    /// `no_log`: whether this task's results are censored on their way to the terminal. Unset
    /// when neither the task, its block nor the play said anything, for the same reason
    /// `ignore_errors` is three-state.
    pub no_log: Option<bool>,
    /// `environment`, one entry per layer that wrote one, outermost first: the play's, then each
    /// block's, then the task's. They are rendered and merged when the task runs, the later
    /// layer winning, because a layer is a mapping whose values may be templates.
    pub environment: Vec<Value>,
    /// `module_defaults`, one mapping per layer that wrote one, outermost first. The innermost
    /// layer naming a module replaces the outer layers' defaults for it as a whole, which is
    /// settled when the task runs, because the values are templates.
    pub module_defaults: Vec<Value>,
    /// `collections`, searched before `ansible.builtin` for a module written by its short name.
    /// `None` when no layer wrote one, `Some` of an empty list for a role that has none: a role
    /// does not inherit the play's.
    pub collections: Option<Vec<String>>,
    /// `check_mode` and `diff`, inherited from the block, the role and the play, whose own
    /// default is the command line's `--check` and `--diff`.
    pub check_mode: Option<Flag>,
    pub diff: Option<Flag>,
    /// `run_once`: one host of the batch runs the task and every other host reads what it
    /// produced. Three-state for the same reason `ignore_errors` is - a block saying `true` and
    /// a task saying `false` inside it have to be told apart from a task that says nothing.
    pub run_once: Option<bool>,
    /// `any_errors_fatal`: a failure of this task on one host fails every host of the batch.
    /// `ignore_unreachable`: an unreachable host stays in the play. Both three-state for the
    /// same reason `ignore_errors` is.
    pub any_errors_fatal: Option<bool>,
    pub ignore_unreachable: Option<bool>,
    /// `delegate_to`, raw: the task runs on that host's connection while everything else about
    /// it - `inventory_hostname`, the registered variable, the recap line - stays with the host
    /// it was written for. Kept as written because it is a template rendered per task.
    pub delegate_facts: Option<bool>,
    pub delegate_to: Option<String>,
    /// Keywords the reference accepts and this release does not execute yet, sorted. A task
    /// carrying one is loaded whole and refused by the pre-flight, never run without it.
    ///
    /// Sorted rather than left in mapping order so the refusal names the same keyword whatever
    /// order the YAML happened to yield: a task carrying two of them would otherwise blame
    /// whichever came first in the file.
    pub unsupported: Vec<&'static str>,
    /// Where the task was written, for the `Origin:` of a failure's error block.
    pub origin: Option<Origin>,
    /// Where some of its values were written - `until`, `retries`, `delay` and a `debug`'s `var` -
    /// for the cause the reference points at when one of them fails.
    ///
    /// Also the action's key, under `action`, and each argument written as a mapping entry,
    /// under `args.<name>`, for the chain a failed render of the arguments prints.
    pub value_origins: std::collections::BTreeMap<String, Origin>,
}

impl PlayTask {
    /// Whether a failure here leaves the host in the play. Silence means no: the keyword is
    /// three-state only so a block and the task under it can disagree, and by the time the
    /// executor reads a task the compiler has already resolved that.
    ///
    /// A template here has not been rendered: `prepare` renders it for the host that runs the
    /// task, and hands the readers a task holding the verdict. One read unrendered is not an
    /// ignored failure.
    pub fn ignores_errors(&self) -> bool {
        self.ignore_errors == Some(Flag::Fixed(true))
    }

    /// A task with nothing in it, for the two shapes the loader builds without reading a
    /// module: a construct the pre-flight is about to refuse, and a block's inherited
    /// keywords. Neither ever reaches an executor as it stands.
    pub(crate) fn empty() -> Self {
        PlayTask {
            name: String::new(),
            named: false,
            module: String::new(),
            args: Map::new(),
            tags: Vec::new(),
            ignore_errors: None,
            timeout: None,
            async_val: 0,
            poll: None,
            throttle: None,
            debugger: None,
            vars: Map::new(),
            when: Vec::new(),
            loop_items: None,
            with_items: false,
            loop_with: None,
            loop_var: "item".to_string(),
            loop_label: None,
            index_var: None,
            loop_extended: None,
            loop_allitems: None,
            loop_pause: None,
            break_when: Vec::new(),
            register: None,
            changed_when: Vec::new(),
            failed_when: Vec::new(),
            r#become: None,
            become_user: None,
            connect: Connect::default(),
            notify: Vec::new(),
            until: Vec::new(),
            retries: None,
            delay: None,
            no_log: None,
            environment: Vec::new(),
            module_defaults: Vec::new(),
            collections: None,
            check_mode: None,
            diff: None,
            run_once: None,
            any_errors_fatal: None,
            ignore_unreachable: None,
            delegate_facts: None,
            delegate_to: None,
            unsupported: Vec::new(),
            origin: None,
            value_origins: std::collections::BTreeMap::new(),
        }
    }

    /// Whether this task's results are censored. Silence means no.
    pub fn censors(&self) -> bool {
        self.no_log.unwrap_or(false)
    }

    /// Whether one host of the batch runs this task for all of them. Silence means no.
    pub fn runs_once(&self) -> bool {
        self.run_once.unwrap_or(false)
    }

    /// Whether the facts this task sets belong to the host it was delegated to rather than to
    /// the host it was written for. Silence means no.
    pub fn delegates_facts(&self) -> bool {
        self.delegate_facts.unwrap_or(false)
    }

    /// Whether one host of the batch runs this task and the others only wait for it: under
    /// `run_once`, and for a `pause`, whose action plugin bypasses the host loop in the
    /// reference. Measured on ansible-core 2.19.12 with two hosts: a `pause` shows `ok: [h1]`
    /// alone, registers nothing on h2, a `when` that leaves h1 out skips it for both, and one
    /// that fails takes h2 out of the play the way a failed `run_once` does. Where the two
    /// differ is `register`, which `run_once` writes for every host: see `fact_targets`. A `meta`
    /// action the reference runs through its first host is the third such task: see
    /// [`PlayTask::meta_once`].
    /// `add_host`'s plugin bypasses the host loop as `pause`'s does: measured, its line is the
    /// batch's first host's alone.
    pub fn bypasses_host_loop(&self) -> bool {
        self.runs_once() || self.pauses() || self.meta_once() || self.adds_host()
    }

    /// Whether this is a `meta` action the reference runs once for the whole batch, through its
    /// first host: its `when` is read on that host alone and the others only wait for the answer.
    /// Measured on ansible-core 2.19.12: `end_play`, `end_batch`, `refresh_inventory`,
    /// `clear_facts` and `clear_host_errors` print one `TASK [meta]` banner, and a `when` that
    /// holds for a later host and not for the first one leaves the play as it was.
    pub fn meta_once(&self) -> bool {
        is_meta(self)
            && self
                .args
                .get("_raw_params")
                .and_then(Value::as_str)
                .is_some_and(|action| META_ONCE.contains(&action))
    }

    fn pauses(&self) -> bool {
        short_name(&self.module) == "pause"
    }

    fn adds_host(&self) -> bool {
        crate::action_plugins::kind(&self.module)
            == Some(crate::action_plugins::Kind::Bridged("add_host"))
    }

    /// Whether the hosts of the batch meet in front of this task before any of them runs it,
    /// because of a keyword written on it, or because it is a `pause`. The play waits out a
    /// pause as a whole, and a host that walked into one with a fork permit kept from the batch
    /// before would hold it for the whole wait, while the hosts behind it cannot start.
    ///
    /// Read out of [`crate::keywords`] rather than hard-coded here: the table is where a
    /// keyword's properties are declared, and a second list of barrier keywords beside it would
    /// be a second thing to keep in step. The textual scan over `hostvars` and the play's host
    /// lists is the executor's own half of the same question and catches what no keyword spells.
    ///
    /// The name is spelled here because a task carries its keywords as fields and not as a list
    /// of names to walk, so this can only ask about the ones it knows. What keeps that honest is
    /// `keywords::tests::run_once_is_the_only_barrier_keyword`: a second row marked `barrier`
    /// would look declared and be ignored here, and that test refuses one.
    pub fn barrier(&self) -> bool {
        (self.runs_once() && crate::keywords::is_barrier("run_once"))
            || self.pauses()
            || self.adds_host()
            || self.fails_all()
            || self.meta_once()
    }

    /// Whether a failure of this task on one host fails the whole batch. A barrier too: the
    /// hosts meet in front of it and learn together how it ended.
    pub fn fails_all(&self) -> bool {
        self.any_errors_fatal == Some(true)
    }

    /// Whether an unreachable host stays in the play after this task.
    pub fn ignores_unreachable(&self) -> bool {
        self.ignore_unreachable == Some(true)
    }
}

/// `remote_user`, `port`, `connection` and the escalation program's keywords `become_method`,
/// `become_exe` and `become_flags`, wherever they are written, as written: each may be a
/// template, rendered when the task runs, except `become_method`, which names one of
/// [`BECOME_METHODS`]. A task's own value wins over its block's and its play's, and a host
/// variable wins over all of them; see `Transport::for_task` and `become_for`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Connect {
    pub remote_user: Option<String>,
    pub port: Option<String>,
    pub connection: Option<String>,
    pub become_method: Option<String>,
    pub become_exe: Option<String>,
    pub become_flags: Option<String>,
}

impl Connect {
    /// This layer over the one outside it.
    pub fn over(&self, outer: &Connect) -> Connect {
        let pick = |inner: &Option<String>, outer: &Option<String>| {
            inner.clone().or_else(|| outer.clone())
        };
        Connect {
            remote_user: pick(&self.remote_user, &outer.remote_user),
            port: pick(&self.port, &outer.port),
            connection: pick(&self.connection, &outer.connection),
            become_method: pick(&self.become_method, &outer.become_method),
            become_exe: pick(&self.become_exe, &outer.become_exe),
            become_flags: pick(&self.become_flags, &outer.become_flags),
        }
    }
}

fn connect_keywords(yaml: &Yaml, context: &str) -> anyhow::Result<Connect> {
    let text = |key: &str| -> anyhow::Result<Option<String>> {
        match field(yaml, key) {
            None | Some(Yaml::Value(Scalar::Null)) => Ok(None),
            Some(Yaml::Value(Scalar::String(s))) => Ok(Some(s.to_string())),
            Some(Yaml::Value(Scalar::Integer(i))) if key == "port" => Ok(Some(i.to_string())),
            Some(other) => bail!("{context}'{key}' must be a name, found {other:?}"),
        }
    };
    let become_method = text("become_method")?;
    if let Some(method) = &become_method
        && !BECOME_METHODS.contains(&method.as_str())
    {
        // Exit 2, not the 4 the rest of a refused playbook gets: measured, the reference reads
        // this keyword happily and then fails the task that would have used it, which is 2.
        return Err(crate::stats::Refusal::at(
            2,
            format!("{context}become_method '{method}' is not supported yet"),
        ));
    }
    Ok(Connect {
        remote_user: text("remote_user")?,
        port: text("port")?,
        connection: text("connection")?,
        become_method,
        become_exe: text("become_exe")?,
        become_flags: text("become_flags")?,
    })
}

/// The escalation methods this release implements. `doas`, `pbrun` and the rest are refused by
/// name: escalating through a different program is not the same operation, and quietly using
/// `sudo` where the playbook asked for `doas` would run the task under rules the operator never
/// wrote.
pub const BECOME_METHODS: [&str; 2] = ["sudo", "su"];

/// The one name in the module column that is not a module: `meta` asks the engine itself for
/// something, so it is read here and compiled into a step of its own instead of travelling to
/// an agent.
pub const META: &str = "meta";

/// `become` and `become_user`, wherever they appear. `become_user` is read as text; `become`
/// goes through `as_bool`, so `yes`, `on` and `"true"` all work as they do in Ansible. The
/// method and its program are read with the connection keywords, which fold the same way.
fn escalation(yaml: &Yaml, context: &str) -> anyhow::Result<(Option<bool>, Option<String>)> {
    let flag = boolean(yaml, "become")?;
    let user = match field(yaml, "become_user") {
        None | Some(Yaml::Value(Scalar::Null)) => None,
        Some(Yaml::Value(Scalar::String(s))) => Some(s.to_string()),
        Some(other) => bail!("{context}'become_user' must be a name, found {other:?}"),
    };
    Ok((flag, user))
}

/// The module an `action:` or `local_action:` value names and the value that module reads its
/// arguments from, as if it had been written under the module's own key.
///
/// Measured on ansible-core 2.19.12: `action: command hostname` and `action: {module: command,
/// cmd: hostname}` both run, the mapping with a deprecation warning, and an `args:` inside the
/// mapping is merged into the arguments beside it. A first word with a `=` in it is a
/// `module=command` spelling the reference cannot resolve, which fails the task rather than the
/// load, and a mapping with no `module:` has "no module/action".
fn action_statement<'a>(node: &Yaml<'a>) -> anyhow::Result<(String, Yaml<'a>)> {
    match node {
        Yaml::Value(Scalar::String(text)) => {
            let text = text.trim();
            let (name, rest) = text
                .split_once(char::is_whitespace)
                .map_or((text, ""), |(name, rest)| (name, rest.trim_start()));
            if name.is_empty() {
                bail!("no module/action detected in task.");
            }
            if name.contains('=') {
                return Err(crate::stats::Refusal::at(
                    2,
                    format!("Cannot resolve '{name}' to an action or module."),
                ));
            }
            let args = if rest.is_empty() {
                Yaml::Value(Scalar::Null)
            } else {
                Yaml::Value(Scalar::String(rest.to_string().into()))
            };
            Ok((name.to_string(), args))
        }
        Yaml::Mapping(entries) => {
            warn_action_mapping(node);
            let mut name = None;
            let mut args = saphyr::Mapping::new();
            for (key, value) in entries {
                match (key.as_str(), value) {
                    (Some("module"), Yaml::Value(Scalar::String(module))) => {
                        name = Some(module.trim().to_string());
                    }
                    (Some("args"), Yaml::Mapping(inner)) => {
                        for (k, v) in inner {
                            args.insert(k.clone(), v.clone());
                        }
                    }
                    _ => {
                        args.insert(key.clone(), value.clone());
                    }
                }
            }
            match name.filter(|name| !name.is_empty()) {
                Some(name) => Ok((name, Yaml::Mapping(args))),
                None => bail!("no module/action detected in task."),
            }
        }
        _ => bail!("no module/action detected in task."),
    }
}

/// A `[DEPRECATION WARNING]:` block on stderr, after the line that says how to silence them,
/// printed once per run.
pub(crate) fn print_deprecation(text: &str) {
    static HEADER: std::sync::Once = std::sync::Once::new();
    HEADER.call_once(|| {
        eprintln!(
            "[WARNING]: Deprecation warnings can be disabled by setting `deprecation_warnings=False` in ansible.cfg."
        );
    });
    eprintln!("[DEPRECATION WARNING]: {text}\n");
}

/// The deprecation the reference prints, on stderr, for a mapping given as an action.
fn warn_action_mapping(node: &Yaml) {
    let origin = value_origin(node)
        .map(|origin| format!("{}\n\n", crate::render::source_context(&origin)))
        .unwrap_or_default();
    print_deprecation(&format!(
        "Using a mapping for `action` is deprecated. This feature will be removed from ansible-core version 2.23.\n{origin}Use a string value for `action`."
    ));
}

/// A keyword whose value has to be a boolean, refused in the reference's own words when it is
/// not one, rather than quietly becoming the default.
///
/// Measured on ansible-core 2.19.12: `gather_facts: maybe` refuses the load at exit 4 with
/// `Error processing keyword 'gather_facts': The value 'maybe' could not be converted to
/// 'bool'.`, and `ignore_errors: maybe` carries that same sentence into the task it fails at
/// run time. `become: maybe` does the same. A default silently put in their place runs the
/// playbook under a value nobody wrote and nobody is told about, which is the family of bug the
/// split between loading and refusing exists to close.
///
/// All three are refused here, at load. That is one step earlier than the reference refuses
/// `ignore_errors` and `become`, and the divergence is deliberate: no task has run yet, so
/// there is nothing half-applied to explain. A template is the one exception, for
/// `ignore_errors`, `check_mode` and `diff`: see [`flag`].
fn boolean(yaml: &Yaml, key: &str) -> anyhow::Result<Option<bool>> {
    match field(yaml, key) {
        None | Some(Yaml::Value(Scalar::Null)) => Ok(None),
        Some(node) => match as_bool(node) {
            Some(b) => Ok(Some(b)),
            None => bail!(
                "Error processing keyword '{key}': The value {} could not be converted to 'bool'.",
                shown(node)
            ),
        },
    }
}

/// A boolean keyword as written: a literal, or a template rendered when the task runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Flag {
    Fixed(bool),
    Template(String),
}

/// `ignore_errors`, `check_mode` and `diff`, which may be templates. Measured on ansible-core 2.19.12:
/// `ignore_errors: "{{ ansible_check_mode }}"`, which `geerlingguy.ntp` and `geerlingguy.docker`
/// write, is rendered when the task runs, so it is kept as written here and rendered by
/// `prepare`. Anything else that is not a boolean is still refused by [`boolean`].
fn flag(yaml: &Yaml, key: &str) -> anyhow::Result<Option<Flag>> {
    match field(yaml, key) {
        Some(Yaml::Value(Scalar::String(s))) if crate::template::Templar::is_template(s) => {
            Ok(Some(Flag::Template(s.to_string())))
        }
        _ => Ok(boolean(yaml, key)?.map(Flag::Fixed)),
    }
}

/// `run_once`, `delegate_facts` and `delegate_to`, wherever the three are written: on a play
/// (`run_once` alone, which is all `Play.fattributes` carries), on a block, on a role entry or
/// on a task.
///
/// `delegate_to` is kept as written. It is a template rendered against the task's own variables
/// when the task runs, and rendering it here would resolve it against nothing.
fn delegation(yaml: &Yaml, context: &str) -> anyhow::Result<(Option<bool>, Option<String>)> {
    let facts = boolean(yaml, "delegate_facts")?;
    let to = match field(yaml, "delegate_to") {
        None | Some(Yaml::Value(Scalar::Null)) => None,
        Some(Yaml::Value(Scalar::String(s))) => Some(s.to_string()),
        Some(other) => bail!("{context}'delegate_to' must be a host name, found {other:?}"),
    };
    Ok((facts, to))
}

/// A scalar the way that refusal shows it: a string in quotes, a number bare, a sequence or
/// mapping the way ansible-core's own Python repr prints it.
///
/// Measured on ansible-core 2.19.12, values checked in
/// `tests::hosts_may_be_a_list_and_gather_facts_defaults_to_true` below: a float is bare with
/// its decimal kept (`1.0`, `3.5`, never a Rust `OrderedFloat` wrapper), a sequence reads
/// `[1, 2]`, a mapping reads `{'a': 1}`, and `null` reads `None`. Ansible's own message for a
/// `null` host entry is a different sentence entirely ("Hosts list cannot contain values of
/// 'None'", not the generic invalid-value sentence this engine reuses for every shape); this
/// engine keeps the one sentence and only fixes the value rendered into it, which is this
/// function's whole job.
fn shown(node: &Yaml) -> String {
    match node {
        Yaml::Value(Scalar::String(s)) => format!("'{s}'"),
        Yaml::Value(Scalar::Integer(i)) => i.to_string(),
        Yaml::Value(Scalar::FloatingPoint(f)) => format!("{:?}", f.into_inner()),
        Yaml::Value(Scalar::Boolean(b)) => if *b { "True" } else { "False" }.to_string(),
        Yaml::Value(Scalar::Null) => "None".to_string(),
        Yaml::Sequence(_) | Yaml::Mapping(_) => {
            to_json(node).map_or_else(|_| format!("{node:?}"), |v| python_repr(&v))
        }
        other => format!("{other:?}"),
    }
}

/// A `serde_json::Value` printed the way Python's own `repr` renders it, since that is what
/// ansible-core's error messages quote a structured value with: strings single-quoted, `null` as
/// `None`, booleans capitalized, everything else bare. Used by [`shown`], for the sequence and
/// mapping shapes `to_json` already knows how to walk, and by the `environment` warning, which
/// quotes the raw stack of layers the same way.
pub(crate) fn python_repr(value: &Value) -> String {
    match value {
        Value::Null => "None".to_string(),
        Value::Bool(b) => if *b { "True" } else { "False" }.to_string(),
        Value::Number(n) => n.to_string(),
        Value::String(s) => python_str_repr(s),
        Value::Array(items) => format!(
            "[{}]",
            items.iter().map(python_repr).collect::<Vec<_>>().join(", ")
        ),
        Value::Object(map) => format!(
            "{{{}}}",
            map.iter()
                .map(|(k, v)| format!("{}: {}", python_str_repr(k), python_repr(v)))
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

/// Python's `repr` of a string: single quotes unless the text holds one and no double quote,
/// the chosen quote and the backslash escaped, control characters as `\n`, `\t`, `\r` or `\xNN`.
// Other characters Python finds unprintable (` `, format characters) go through
// as they are; escape them by Unicode category if a label ever needs it.
fn python_str_repr(s: &str) -> String {
    use std::fmt::Write as _;
    let quote = if s.contains('\'') && !s.contains('"') {
        '"'
    } else {
        '\''
    };
    let mut out = String::with_capacity(s.len() + 2);
    out.push(quote);
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c == quote => {
                out.push('\\');
                out.push(c);
            }
            c if c.is_control() && u32::from(c) < 0x100 => {
                let _ = write!(out, "\\x{:02x}", u32::from(c));
            }
            c => out.push(c),
        }
    }
    out.push(quote);
    out
}

/// Whether this task asks the engine for something instead of naming a module to run.
pub fn is_meta(task: &PlayTask) -> bool {
    short_name(&task.module) == META
}

/// The `meta` actions that run once for the batch; see [`PlayTask::meta_once`].
pub(crate) const META_ONCE: [&str; 5] = [
    "clear_facts",
    "clear_host_errors",
    "end_batch",
    "end_play",
    "refresh_inventory",
];

/// The collection modules the reference reads a string argument of as one command line: the two
/// of `ansible.windows` in ansible-core 2.19.12's `FREEFORM_ACTIONS`, whose other entries are all
/// builtin names.
pub(crate) const FREE_FORM_COLLECTION_MODULES: &[&str] =
    &["ansible.windows.win_command", "ansible.windows.win_shell"];

/// The builtin actions ansible-core 2.19.12 lets keep a string argument that is not `key=value`
/// (`_BUILTIN_RAW_PARAM_MODULES_SIMPLE` in `parsing/mod_args.py`); its free-form modules are
/// [`is_free_form`]'s. `set_fact: foo` sets a fact named `_raw_params` there, and here.
const BUILTIN_RAW_PARAM_MODULES: [&str; 9] = [
    "include_vars",
    "include_tasks",
    "include_role",
    "import_tasks",
    "import_role",
    "add_host",
    "group_by",
    "set_fact",
    "meta",
];

/// What the loader kept of a module's string argument that is not `key=value`, when the module
/// does not read that string as a command line or as its own raw parameters.
pub(crate) fn stray_raw_params(task: &PlayTask) -> Option<&str> {
    let takes_it = if crate::python::is_resolved_name(&task.module) {
        FREE_FORM_COLLECTION_MODULES.contains(&task.module.as_str())
    } else {
        is_free_form(&task.module) || BUILTIN_RAW_PARAM_MODULES.contains(&short_name(&task.module))
    };
    if takes_it {
        return None;
    }
    task.args
        .get("_raw_params")
        .and_then(Value::as_str)
        .filter(|raw| !raw.is_empty())
}

/// ansible-core's `is_possibly_all_template`: a string that opens with a Jinja override header,
/// or that opens and closes with Jinja delimiters, whichever pair.
pub(crate) fn possibly_all_template(text: &str) -> bool {
    text.starts_with("#jinja2:")
        || (["{%", "{{", "{#"].iter().any(|m| text.starts_with(m))
            && ["%}", "}}", "#}"].iter().any(|m| text.ends_with(m)))
}

/// Whether the module's string form is one command line rather than `key=value` pairs.
///
/// The three registries answer for their own rows, so a module moving between them keeps the
/// form its table gives it. `include_tasks: sub.yml` and `include_vars: v.yml` name a file the
/// way `import_tasks: sub.yml` does, and `include_role` wants `name=` like `import_role`.
fn is_free_form(module: &str) -> bool {
    FREE_FORM_COLLECTION_MODULES.contains(&module)
        || native(module).is_some_and(|m| m.free_form)
        || local(module).is_some_and(|m| m.free_form)
        || import_module(module) == Some(true)
        || include_module(module) == Some(true)
        // A command line like `command`'s, in the reference's `MODULE_REQUIRE_ARGS`.
        || crate::action_plugins::kind(module)
            == Some(crate::action_plugins::Kind::Bridged("script"))
}

/// Reads and parses one playbook. A file that is not there stops the run with exit 1; a file
/// that is there and does not make sense stops it with exit 4. Both codes are the reference's
/// own, measured against ansible-core 2.19.12: `the playbook: x.yml could not be found` exits 1,
/// while a YAML error, a play that is not a mapping, an unknown keyword, a module that cannot be
/// resolved and `a playbook must be a list of plays` all exit 4, with no `PLAY RECAP`.
///
/// Module names are not resolved here. The reference resolves an action while it loads the
/// play, and this engine still refuses an unresolvable one before the first task, but one step
/// later: [`crate::preflight`] does it, together with the keywords the reference accepts and
/// this release cannot execute. Loading and refusing are separated so a playbook parses and
/// lists exactly as it does in the reference, while the run still stops before it can apply
/// half of itself.
pub fn load(path: &Path) -> anyhow::Result<Playbook> {
    read(path, 0).map_err(|err| crate::stats::Refusal::or(4, err))
}

/// How deep `import_playbook` may nest before the run is refused. A file importing itself would
/// otherwise read on until the process ran out of stack, and a refusal naming the depth says
/// which file to look at.
const IMPORT_DEPTH: usize = 32;

fn read(path: &Path, depth: usize) -> anyhow::Result<Playbook> {
    // Exit 1 for a file that is not there, measured, and not the 4 a file that is there and
    // makes no sense gets. It carries its own code so the blanket [`load`] wraps the parse in
    // cannot take it.
    let text = crate::vault::read_to_string(path).map_err(|err| {
        crate::stats::Refusal::at(1, format!("reading playbook {}: {err}", path.display()))
    })?;
    parse_at(&text, &path.display().to_string(), &base_dir(path), depth)
}

/// A playbook's own directory, absolute where the filesystem allows it: `group_vars/`,
/// `host_vars/`, relative `vars_files` entries, the `roles/` directory, lookups and the
/// `playbook_dir` variable all resolve against it. An empty parent means the file was named with
/// no directory at all, so it is the working directory.
pub(crate) fn base_dir(path: &Path) -> PathBuf {
    let dir = match path.parent() {
        Some(p) if !p.as_os_str().is_empty() => p.to_path_buf(),
        _ => PathBuf::from("."),
    };
    dir.canonicalize().unwrap_or(dir)
}

pub fn parse(text: &str, source: &str) -> anyhow::Result<Playbook> {
    parse_at(text, source, &base_dir(Path::new(source)), 0)
}

/// One playbook, with every `import_playbook` entry replaced in place by the plays of the file
/// it names.
///
/// In place, not appended: measured on ansible-core 2.19.12, a file importing `sub.yml`, then
/// declaring a play of its own, then importing `sub.yml` again runs the three plays in the order
/// they are written. Each imported play keeps the directory of the file it came from, so its
/// roles and its `vars_files` are looked for beside that file and not beside the playbook the
/// operator typed.
fn parse_at(text: &str, source: &str, dir: &Path, depth: usize) -> anyhow::Result<Playbook> {
    let (docs, origins) = crate::yaml::load_with_origins(text, source)?;
    let _origins = OriginScope::enter(source, origins);
    let Some(Yaml::Sequence(entries)) = docs.first() else {
        bail!("{source}: a playbook must be a list of plays");
    };
    let mut plays = Vec::new();
    for (i, entry) in entries.iter().enumerate() {
        match imported_playbook(entry)? {
            Some(name) => {
                if depth >= IMPORT_DEPTH {
                    bail!("{source}: 'import_playbook' nests deeper than {IMPORT_DEPTH} levels");
                }
                let path = Path::new(&name);
                let path = if path.is_absolute() {
                    path.to_path_buf()
                } else {
                    dir.join(path)
                };
                plays.extend(
                    read(&path, depth + 1)
                        .with_context(|| format!("{source}: import_playbook {name}"))?
                        .plays,
                );
            }
            None => plays
                .push(parse_play(entry, dir).with_context(|| format!("{source}: play {}", i + 1))?),
        }
    }
    Ok(Playbook { plays })
}

/// The file an `import_playbook` entry names, rendered.
///
/// The reference renders the value with no variables at all - measured, `{{ 'sub' }}.yml` works
/// and `{{ nosuchvar }}.yml` refuses the load with `Error processing keyword 'import_playbook':
/// 'nosuchvar' is undefined`, exit 4. Nothing has an inventory yet at this point, so a name that
/// depends on a host could not be answered anyway.
fn imported_playbook(entry: &Yaml) -> anyhow::Result<Option<String>> {
    let Some(map) = entry.as_mapping() else {
        return Ok(None);
    };
    let value = map.iter().find_map(|(key, value)| {
        matches!(
            key.as_str(),
            Some("import_playbook" | "ansible.builtin.import_playbook")
        )
        .then_some(value)
    });
    let Some(value) = value else {
        return Ok(None);
    };
    let raw = value
        .as_str()
        .ok_or_else(|| anyhow!("'import_playbook' takes a file name"))?;
    if !crate::template::Templar::is_template(raw) {
        return Ok(Some(raw.to_string()));
    }
    let templar = crate::template::Templar::new(PathBuf::from("."));
    let rendered = templar
        .render(raw, &Map::new())
        .map_err(|err| anyhow!("Error processing keyword 'import_playbook': {}", err.0))?;
    match rendered {
        Value::String(name) => Ok(Some(name)),
        other => bail!("Error processing keyword 'import_playbook': {other} is not a file name"),
    }
}

fn parse_play(yaml: &Yaml, dir: &Path) -> anyhow::Result<Play> {
    let map = yaml
        .as_mapping()
        .ok_or_else(|| anyhow!("a play must be a mapping"))?;
    let mut unsupported = Vec::new();
    for (key, _) in map {
        let key = key.as_str().unwrap_or_default();
        // The reference's own words, measured: an attribute it does not have refuses the load
        // with exit 4, and one it has but this release cannot execute is kept for the
        // pre-flight instead of being refused here.
        match play_keyword(key) {
            None => bail!("'{key}' is not a valid attribute for a Play"),
            Some(k) if k.support == Support::Preflight => unsupported.push(k.name),
            Some(_) => {}
        }
    }
    unsupported.sort_unstable();
    let host_patterns = match field(yaml, "hosts") {
        Some(Yaml::Value(Scalar::String(s))) => vec![s.to_string()],
        // Every entry, or none: `filter_map` here dropped a non-string entry and ran the play
        // against the rest, which is a host list quietly shorter than the one written. The
        // reference refuses it instead, measured: `Hosts list contains an invalid host value:
        // '3'`, exit 4.
        Some(Yaml::Sequence(items)) => items
            .iter()
            .map(|i| {
                i.as_str().map(str::to_string).ok_or_else(|| {
                    anyhow!("Hosts list contains an invalid host value: '{}'", shown(i))
                })
            })
            .collect::<anyhow::Result<Vec<_>>>()?,
        None => bail!("a play needs 'hosts'"),
        Some(other) => bail!("'hosts' must be a string or a list, found {other:?}"),
    };
    let hosts = host_patterns.join(",");
    let name = field(yaml, "name")
        .and_then(Yaml::as_str)
        .unwrap_or(&hosts)
        .to_string();
    let gather_written = boolean(yaml, "gather_facts")?;
    let gather_facts = gather_written.unwrap_or(true);
    let vars = match field(yaml, "vars") {
        None | Some(Yaml::Value(Scalar::Null)) => Map::new(),
        Some(v) => match to_json(v).context("'vars'")? {
            Value::Object(map) => map,
            _ => bail!("'vars' must be a mapping"),
        },
    };
    let vars_files = match field(yaml, "vars_files") {
        None | Some(Yaml::Value(Scalar::Null)) => Vec::new(),
        Some(Yaml::Sequence(items)) => items
            .iter()
            .map(|i| {
                i.as_str()
                    .map(str::to_string)
                    .ok_or_else(|| anyhow!("'vars_files' entries must be strings"))
            })
            .collect::<anyhow::Result<_>>()?,
        Some(Yaml::Value(Scalar::String(s))) => vec![s.to_string()],
        _ => bail!("'vars_files' must be a list of paths"),
    };
    let tasks = task_list(field(yaml, "tasks"), "tasks")?;
    let pre_tasks = task_list(field(yaml, "pre_tasks"), "pre_tasks")?;
    let post_tasks = task_list(field(yaml, "post_tasks"), "post_tasks")?;
    let roles = match field(yaml, "roles") {
        None | Some(Yaml::Value(Scalar::Null)) => Vec::new(),
        Some(node) => parse_role_entries(node).context("'roles'")?,
    };
    let strategy = match field(yaml, "strategy") {
        None | Some(Yaml::Value(Scalar::Null)) => None,
        Some(Yaml::Value(Scalar::String(s))) => Some(s.to_string()),
        Some(other) => bail!("'strategy' must be a name, found {other:?}"),
    };
    let order = match field(yaml, "order") {
        None | Some(Yaml::Value(Scalar::Null)) => None,
        Some(Yaml::Value(Scalar::String(s))) => Some(s.to_string()),
        Some(Yaml::Value(Scalar::Integer(i))) => Some(i.to_string()),
        Some(other) => bail!("'order' must be a name, found {other:?}"),
    };
    // Kept as it was written, template included: the reference renders it when the play starts,
    // against the play's own variables and the run's extra variables.
    let serial = match field(yaml, "serial") {
        None | Some(Yaml::Value(Scalar::Null)) => None,
        Some(v) => Some(to_json(v).context("'serial'")?),
    };
    let (r#become, become_user) = escalation(yaml, "")?;
    Ok(Play {
        origin: task_origin(map),
        name,
        hosts,
        host_patterns,
        tags: tags(yaml, "")?,
        gather_facts,
        gather_written: gather_written.is_some(),
        vars,
        vars_files,
        tasks,
        pre_tasks,
        post_tasks,
        roles,
        handlers: parse_handlers(field(yaml, "handlers"))?,
        force_handlers: boolean(yaml, "force_handlers")?,
        dir: dir.to_path_buf(),
        r#become,
        become_user,
        connect: connect_keywords(yaml, "")?,
        strategy,
        order,
        timeout: timeout(yaml, "")?,
        throttle: throttle(yaml, "")?,
        debugger: debugger(yaml, "")?,
        serial,
        no_log: boolean(yaml, "no_log")?,
        environment: environment(yaml, "")?,
        module_defaults: module_defaults(yaml, "")?,
        collections: collections(yaml, "")?,
        check_mode: flag(yaml, "check_mode")?,
        diff: flag(yaml, "diff")?,
        run_once: boolean(yaml, "run_once")?,
        any_errors_fatal: boolean(yaml, "any_errors_fatal")?,
        ignore_errors: flag(yaml, "ignore_errors")?,
        ignore_unreachable: boolean(yaml, "ignore_unreachable")?,
        max_fail_percentage: match field(yaml, "max_fail_percentage") {
            None | Some(Yaml::Value(Scalar::Null)) => None,
            Some(v) => Some(to_json(v).context("'max_fail_percentage'")?),
        },
        gather_subset: raw_keyword(yaml, "gather_subset")?,
        gather_timeout: raw_keyword(yaml, "gather_timeout")?,
        fact_path: raw_keyword(yaml, "fact_path")?,
        vars_prompt: vars_prompt(yaml)?,
        unsupported,
    })
}

/// A keyword kept as written, `None` when it is absent or null.
fn raw_keyword(yaml: &Yaml, key: &str) -> anyhow::Result<Option<Value>> {
    match field(yaml, key) {
        None | Some(Yaml::Value(Scalar::Null)) => Ok(None),
        Some(v) => Ok(Some(to_json(v).with_context(|| format!("'{key}'"))?)),
    }
}

/// The keys of a `vars_prompt` entry, as the reference's loader lists them.
const PROMPT_KEYS: &[&str] = &[
    "name",
    "prompt",
    "default",
    "private",
    "confirm",
    "encrypt",
    "salt_size",
    "salt",
    "unsafe",
];

/// The play's `vars_prompt`: a list of mappings, or one mapping, each with a `name` and no key
/// the reference does not know.
fn vars_prompt(yaml: &Yaml) -> anyhow::Result<Vec<Map<String, Value>>> {
    let entries = match raw_keyword(yaml, "vars_prompt")? {
        None => return Ok(Vec::new()),
        Some(Value::Array(items)) => items,
        Some(one) => vec![one],
    };
    entries
        .into_iter()
        .map(|entry| {
            let Value::Object(map) = entry else {
                bail!("Invalid vars_prompt data structure, expected a mapping per entry");
            };
            if !map.contains_key("name") {
                bail!("Invalid vars_prompt data structure, missing 'name' key");
            }
            if let Some(key) = map.keys().find(|k| !PROMPT_KEYS.contains(&k.as_str())) {
                bail!("Invalid vars_prompt data structure, found unsupported key '{key}'");
            }
            Ok(map)
        })
        .collect()
}

/// `environment`, wherever it is written: at most one layer per level, kept raw because its
/// values are templates the task renders against its own variables.
///
/// The value is not required to be a mapping here. Measured on ansible-core 2.19.12: a value
/// that is not one warns and is skipped while the task still runs, so refusing the load would
/// refuse a playbook the reference accepts.
fn environment(yaml: &Yaml, context: &str) -> anyhow::Result<Vec<Value>> {
    match field(yaml, "environment") {
        None | Some(Yaml::Value(Scalar::Null)) => Ok(Vec::new()),
        Some(v) => Ok(vec![
            to_json(v).with_context(|| format!("{context}'environment'"))?,
        ]),
    }
}

/// `module_defaults`, wherever it is written: one layer per level, a mapping or a list of
/// mappings. The keys are action or group names and are read as they are; the values are
/// templates, rendered when a task runs.
fn module_defaults(yaml: &Yaml, context: &str) -> anyhow::Result<Vec<Value>> {
    let layers = match field(yaml, "module_defaults") {
        None | Some(Yaml::Value(Scalar::Null)) => return Ok(Vec::new()),
        Some(Yaml::Sequence(items)) => items.iter().collect(),
        Some(node) => vec![node],
    };
    layers
        .into_iter()
        .map(|node| match to_json(node).with_context(|| format!("{context}'module_defaults'"))? {
            layer @ Value::Object(_) => Ok(layer),
            _ => bail!(
                "{context}The field 'module_defaults' is supposed to be a dictionary or list of dictionaries, the keys of which must be static action, module, or group names. Only the values may contain templates. For example: {{'ping': \"{{{{ ping_defaults }}}}\"}}"
            ),
        })
        .collect()
}

/// `collections`, wherever it is written: a name or a list of names, searched in order. Absent
/// is `None`, so the layer above is not hidden; a null or an empty list says none.
fn collections(yaml: &Yaml, context: &str) -> anyhow::Result<Option<Vec<String>>> {
    match field(yaml, "collections") {
        None => Ok(None),
        Some(_) => names(yaml, "collections", context).map(Some),
    }
}

/// A play's `roles:` list, or the `dependencies:` of a `meta/main.yml`. The two are the same
/// grammar, read by the same code so they cannot drift: measured, a dependency carries its own
/// parameters and runs before the role that depends on it.
pub(crate) fn parse_role_entries(node: &Yaml) -> anyhow::Result<Vec<RoleEntry>> {
    let Yaml::Sequence(items) = node else {
        bail!("'roles' must be a list of roles");
    };
    items
        .iter()
        .enumerate()
        .map(|(i, item)| parse_role_entry(item).with_context(|| format!("role {}", i + 1)))
        .collect()
}

/// One role entry: a bare name, or a mapping naming the role and carrying keywords, the four
/// `*_from` selectors, and parameters.
///
/// The split between a keyword and a parameter is measured, not guessed: a free key on the entry
/// is a role parameter and beats a `set_fact` on the same name, while `vars:` on the entry does
/// not - so `vars:` is read as the block keyword it is and only the leftovers are parameters.
fn parse_role_entry(yaml: &Yaml) -> anyhow::Result<RoleEntry> {
    if let Some(name) = yaml.as_str() {
        return Ok(RoleEntry {
            name: name.to_string(),
            from: RoleFrom::default(),
            params: Map::new(),
            keywords: PlayTask::empty(),
        });
    }
    let map = yaml
        .as_mapping()
        .ok_or_else(|| anyhow!("a role must be a name or a mapping"))?;
    let name = match field(yaml, "role").or_else(|| field(yaml, "name")) {
        Some(Yaml::Value(Scalar::String(s))) => s.to_string(),
        Some(other) => bail!("'role' must be a name, found {other:?}"),
        None => bail!("a role entry needs 'role'"),
    };
    let mut from = RoleFrom::default();
    let mut params = Map::new();
    let mut unsupported = Vec::new();
    for (key, value) in map {
        let key = key
            .as_str()
            .ok_or_else(|| anyhow!("role '{name}': keys must be strings"))?;
        let selector = match key {
            "tasks_from" => Some(&mut from.tasks),
            "vars_from" => Some(&mut from.vars),
            "defaults_from" => Some(&mut from.defaults),
            "handlers_from" => Some(&mut from.handlers),
            _ => None,
        };
        if let Some(slot) = selector {
            *slot = value
                .as_str()
                .ok_or_else(|| anyhow!("role '{name}': '{key}' must be a file name"))?
                .to_string();
            continue;
        }
        if key == "role" || key == "name" {
            continue;
        }
        // A role entry takes the keywords a block takes, which is the list the reference checks
        // one against; anything else is a parameter for the role itself.
        match block_keyword(key) {
            Some(k) if k.support == Support::Preflight => unsupported.push(k.name),
            Some(_) => {}
            None => {
                params.insert(key.to_string(), to_json(value)?);
            }
        }
    }
    unsupported.sort_unstable();
    let context = format!("role '{name}': ");
    let (r#become, become_user) = escalation(yaml, &context)?;
    let (delegate_facts, delegate_to) = delegation(yaml, &context)?;
    Ok(RoleEntry {
        name,
        from,
        params,
        keywords: PlayTask {
            name: String::new(),
            tags: tags(yaml, &context)?,
            ignore_errors: flag(yaml, "ignore_errors")?,
            timeout: timeout(yaml, &context)?,
            throttle: throttle(yaml, &context)?,
            debugger: debugger(yaml, &context)?,
            vars: mapping(yaml, "vars", &context)?,
            when: conditions(yaml, "when", &context)?,
            r#become,
            become_user,
            connect: connect_keywords(yaml, &context)?,
            notify: names(yaml, "notify", &context)?,
            no_log: boolean(yaml, "no_log")?,
            environment: environment(yaml, &context)?,
            module_defaults: module_defaults(yaml, &context)?,
            collections: collections(yaml, &context)?,
            check_mode: flag(yaml, "check_mode")?,
            diff: flag(yaml, "diff")?,
            run_once: boolean(yaml, "run_once")?,
            any_errors_fatal: boolean(yaml, "any_errors_fatal")?,
            ignore_unreachable: boolean(yaml, "ignore_unreachable")?,
            delegate_facts,
            delegate_to,
            unsupported,
            ..PlayTask::empty()
        },
    })
}

/// The task list of one file, for a role's `tasks/main.yml` and for `import_tasks`.
///
/// Measured on ansible-core 2.19.12: a file whose document is a mapping rather than a list is
/// refused with `included task files must contain a list of tasks`, exit 4, and one that is not
/// there at all is a different refusal with a different code, raised by the caller that knows
/// which path it asked for.
pub(crate) fn parse_tasks_file(path: &Path) -> anyhow::Result<Vec<TaskOrBlock>> {
    parse_tasks_text(path).map_err(|err| {
        if err.downcast_ref::<NotATaskList>().is_some() {
            err
        } else {
            anyhow::Error::new(TaskFileRefused(format!("{err:#}")))
        }
    })
}

/// A task file the reference refuses while it loads it: a YAML error, a task it cannot parse.
/// Raised from a dynamic include it ends the run at exit 4 with the statement's banner and no
/// recap; measured on ansible-core 2.19.12, unlike [`NotATaskList`], which fails the host.
#[derive(Debug)]
pub(crate) struct TaskFileRefused(pub String);

impl std::fmt::Display for TaskFileRefused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for TaskFileRefused {}

/// A task file whose document is not a list.
#[derive(Debug)]
pub(crate) struct NotATaskList(String);

impl std::fmt::Display for NotATaskList {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}: included task files must contain a list of tasks",
            self.0
        )
    }
}

impl std::error::Error for NotATaskList {}

fn parse_tasks_text(path: &Path) -> anyhow::Result<Vec<TaskOrBlock>> {
    let text = crate::vault::read_to_string(path)
        .with_context(|| format!("reading {}", path.display()))?;
    let source = path.display().to_string();
    let (docs, origins) = crate::yaml::load_with_origins(&text, &source)?;
    let _origins = OriginScope::enter(&source, origins);
    match docs.first() {
        None | Some(Yaml::Value(Scalar::Null)) => Ok(Vec::new()),
        Some(node @ Yaml::Sequence(_)) => {
            task_list(Some(node), "tasks").with_context(|| source.clone())
        }
        Some(_) => Err(NotATaskList(source).into()),
    }
}

/// The handler list of one file, for a role's `handlers/main.yml`.
pub(crate) fn parse_handlers_file(path: &Path) -> anyhow::Result<Vec<HandlerTask>> {
    let text = crate::vault::read_to_string(path)
        .with_context(|| format!("reading {}", path.display()))?;
    let source = path.display().to_string();
    let (docs, origins) = crate::yaml::load_with_origins(&text, &source)?;
    let _origins = OriginScope::enter(&source, origins);
    match docs.first() {
        None | Some(Yaml::Value(Scalar::Null)) => Ok(Vec::new()),
        Some(node @ Yaml::Sequence(_)) => {
            parse_handlers(Some(node)).with_context(|| source.clone())
        }
        Some(_) => bail!("{source}: a handlers file must contain a list of handlers"),
    }
}

/// One list of tasks and blocks: a play's `tasks`, or one of a block's three sections. An
/// absent or null list is empty, which is what the reference does with `block: []` - measured,
/// it runs the playbook and shows nothing for the block.
fn task_list(node: Option<&Yaml>, label: &str) -> anyhow::Result<Vec<TaskOrBlock>> {
    match node {
        Some(Yaml::Sequence(items)) => items
            .iter()
            .enumerate()
            .map(|(i, y)| parse_item(y).with_context(|| format!("{label} item {}", i + 1)))
            .collect(),
        None | Some(Yaml::Value(Scalar::Null)) => Ok(Vec::new()),
        _ => bail!("'{label}' must be a list"),
    }
}

/// A task, or a block. A mapping carrying `block`, `rescue` or `always` is a block: the
/// reference decides it the same way, which is why `rescue` on its own is an error about a
/// missing `block` rather than an unknown module.
fn parse_item(yaml: &Yaml) -> anyhow::Result<TaskOrBlock> {
    let map = yaml
        .as_mapping()
        .ok_or_else(|| anyhow!("a task must be a mapping"))?;
    let has = |name: &str| map.keys().any(|k| k.as_str() == Some(name));
    if has("block") {
        return Ok(TaskOrBlock::Block(parse_block(yaml)?));
    }
    // Measured on ansible-core 2.19.12, exit 4, one sentence per section:
    // `'rescue' keyword cannot be used without 'block'`.
    for section in BLOCK_SECTIONS.iter().filter(|s| **s != "block") {
        if has(section) {
            bail!("'{section}' keyword cannot be used without 'block'");
        }
    }
    Ok(TaskOrBlock::Task(parse_task(yaml, false)?))
}

/// A block, with its three sections parsed recursively and its inherited keywords read into a
/// module-less [`PlayTask`].
///
/// Every key is checked against the reference's own Block attribute list, so a key that belongs
/// on a task and not on a block - `loop`, `register`, `until` - is refused in the reference's
/// words instead of being taken for a module or quietly ignored.
fn parse_block(yaml: &Yaml) -> anyhow::Result<Block> {
    let map = yaml
        .as_mapping()
        .ok_or_else(|| anyhow!("a block must be a mapping"))?;
    let name = field(yaml, "name")
        .and_then(Yaml::as_str)
        .unwrap_or("block")
        .to_string();
    let mut unsupported = Vec::new();
    for (key, _) in map {
        let key = key
            .as_str()
            .ok_or_else(|| anyhow!("block '{name}': keys must be strings"))?;
        match block_keyword(key) {
            // The reference's own words, measured: a module key next to `block:` reads as an
            // attribute the block does not have, `'debug' is not a valid attribute for a Block`.
            None => bail!("'{key}' is not a valid attribute for a Block"),
            Some(k) if k.support == Support::Preflight => unsupported.push(k.name),
            Some(_) => {}
        }
    }
    unsupported.sort_unstable();
    let context = format!("block '{name}': ");
    let (r#become, become_user) = escalation(yaml, &context)?;
    let (delegate_facts, delegate_to) = delegation(yaml, &context)?;
    Ok(Block {
        body: task_list(field(yaml, "block"), "block")?,
        rescue: task_list(field(yaml, "rescue"), "rescue")?,
        always: task_list(field(yaml, "always"), "always")?,
        keywords: PlayTask {
            name,
            tags: tags(yaml, &context)?,
            ignore_errors: flag(yaml, "ignore_errors")?,
            timeout: timeout(yaml, &context)?,
            throttle: throttle(yaml, &context)?,
            debugger: debugger(yaml, &context)?,
            vars: mapping(yaml, "vars", &context)?,
            when: conditions(yaml, "when", &context)?,
            r#become,
            become_user,
            connect: connect_keywords(yaml, &context)?,
            notify: names(yaml, "notify", &context)?,
            no_log: boolean(yaml, "no_log")?,
            environment: environment(yaml, &context)?,
            module_defaults: module_defaults(yaml, &context)?,
            collections: collections(yaml, &context)?,
            check_mode: flag(yaml, "check_mode")?,
            diff: flag(yaml, "diff")?,
            run_once: boolean(yaml, "run_once")?,
            any_errors_fatal: boolean(yaml, "any_errors_fatal")?,
            ignore_unreachable: boolean(yaml, "ignore_unreachable")?,
            delegate_facts,
            delegate_to,
            unsupported,
            ..PlayTask::empty()
        },
    })
}

/// One task. `handler` says whether the mapping is a handler, which is the one shape that may
/// carry `listen`: read here so the key is not mistaken for a module, and read from its own table
/// so the row can be flipped like any other.
fn parse_task(yaml: &Yaml, handler: bool) -> anyhow::Result<PlayTask> {
    let map = yaml
        .as_mapping()
        .ok_or_else(|| anyhow!("a task must be a mapping"))?;
    let name = field(yaml, "name")
        .and_then(Yaml::as_str)
        .map(str::to_string);
    let label = name.clone().unwrap_or_else(|| "unnamed".to_string());
    let mut module: Option<(String, &Yaml)> = None;
    // `action:` or `local_action:`, the spellings that name the module in their value.
    let mut statement: Option<(&'static str, &Yaml)> = None;
    let mut unsupported = Vec::new();
    for (key, value) in map {
        let key = key
            .as_str()
            .ok_or_else(|| anyhow!("task '{label}': keys must be strings"))?;
        if handler && let Some(k) = handler_keyword(key) {
            if k.support == Support::Preflight {
                unsupported.push(k.name);
            }
            continue;
        }
        match task_keyword(key) {
            Some(k) if k.support == Support::Preflight => unsupported.push(k.name),
            Some(k) if matches!(k.name, "action" | "local_action") => {
                if statement.is_some() {
                    bail!("action and local_action are mutually exclusive");
                }
                statement = Some((k.name, value));
            }
            Some(_) => {}
            // Whatever is left is the module, which is how the reference reads a task too: a
            // key it does not know is an action, and a second one is the conflict below rather
            // than an unknown-keyword error.
            None => {
                if let Some((first, _)) = &module {
                    bail!("conflicting action statements: {first}, {key}");
                }
                module = Some((key.to_string(), value));
            }
        }
    }
    // `loop_control` is a mapping, so it has a grammar of its own. The reference refuses a
    // sub-key it does not know at load - measured, exit 4, `'nosuch' is not a valid attribute
    // for a LoopControl` - and one it knows and this release cannot honour would be parked for
    // the pre-flight like any other keyword.
    let mut loop_var = "item".to_string();
    let mut loop_label = None;
    let mut index_var = None;
    let mut loop_extended = None;
    let mut loop_allitems = None;
    let mut loop_pause = None;
    let mut break_when = Vec::new();
    if let Some(control) = field(yaml, "loop_control") {
        let control = control
            .as_mapping()
            .ok_or_else(|| anyhow!("task '{label}': 'loop_control' must be a mapping"))?;
        for (key, value) in control {
            let key = key
                .as_str()
                .ok_or_else(|| anyhow!("task '{label}': 'loop_control' keys must be strings"))?;
            let Some(k) = loop_control_keyword(key) else {
                bail!("'{key}' is not a valid attribute for a LoopControl");
            };
            if k.support == Support::Preflight {
                unsupported.push(k.name);
                continue;
            }
            let name = || {
                value.as_str().map(str::to_string).ok_or_else(|| {
                    anyhow!("task '{label}': 'loop_control.{}' must be a name", k.name)
                })
            };
            match k.name {
                "loop_var" => loop_var = name()?,
                "label" => loop_label = Some(name()?),
                "index_var" => index_var = Some(name()?),
                "extended" => loop_extended = Some(to_json(value)?),
                "extended_allitems" => loop_allitems = Some(to_json(value)?),
                "pause" => loop_pause = Some(to_json(value)?),
                "break_when" => {
                    let context = format!("task '{label}': ");
                    break_when = match value {
                        Yaml::Sequence(items) => items
                            .iter()
                            .map(|n| condition(n, "break_when", &context))
                            .collect::<anyhow::Result<_>>()?,
                        node => vec![condition(node, "break_when", &context)?],
                    };
                }
                other => unreachable!("loop_control sub-key '{other}' has no reader"),
            }
        }
    }
    unsupported.sort_unstable();
    // The statement's value is the action: a string is `module args`, a mapping names the module
    // under `module:` and holds its arguments beside it. It is read after the whole mapping so a
    // conflict is reported the same whichever key came first.
    let rewritten;
    let mut local = false;
    let source = statement.map(|(_, node)| node);
    let module = match statement {
        Some((word, node)) => {
            let (name, args) = action_statement(node)?;
            if let Some((other, _)) = &module {
                bail!("conflicting action statements: {name}, {other}");
            }
            local = word == "local_action";
            rewritten = args;
            Some((name, &rewritten))
        }
        None => module,
    };
    // A task whose only content this release cannot execute has no module to find - `block:`
    // is the spelling - so it is loaded empty and refused whole by
    // the pre-flight, instead of being blamed for a module it was never asked to name.
    let (module, value) = match module {
        Some(found) => found,
        None if !unsupported.is_empty() => {
            return Ok(PlayTask {
                name: label.clone(),
                named: name.is_some(),
                module: String::new(),
                tags: tags(yaml, &format!("task '{label}': "))?,
                unsupported,
                ..PlayTask::empty()
            });
        }
        None => bail!("task '{label}': no module given"),
    };
    // Arguments are read only for a module this release can run. The reference resolves the
    // action before it looks at what was handed to it, so a name it cannot resolve is refused
    // for its name and never for the shape of arguments nobody promised to read; the pre-flight
    // raises that refusal a moment later. Reading them anyway made `nosuchmodule: echo a`
    // complain about `key=value`, which sends the operator to the wrong line.
    let mut args = if short_name(&module) == META {
        // `meta` is not a module the agent runs, so its argument is read here and nowhere else:
        // the compiler turns the task into a step of its own and the pre-flight decides whether
        // this release honours the action.
        let action = value
            .as_str()
            .ok_or_else(|| anyhow!("task '{label}': 'meta' takes an action name"))?;
        let mut args = Map::new();
        args.insert("_raw_params".into(), Value::String(action.to_string()));
        args
    } else if is_known(&module)
        || crate::python::is_python_module(&module)
        || crate::action_plugins::kind(&module).is_some()
        || import_module(&module).is_some()
        || include_module(&module).is_some()
    {
        // The import and include statements are read here for the same reason `meta` is: the
        // compiler needs what they name, or the coordinator does once the play is running, and
        // nothing else ever will, because no step is left for a host.
        module_args(&module, value).with_context(|| format!("task '{label}'"))?
    } else {
        Map::new()
    };
    if let Some(Yaml::Mapping(extra)) = field(yaml, "args") {
        for (k, v) in extra {
            let k = k
                .as_str()
                .ok_or_else(|| anyhow!("task '{label}': 'args' keys must be strings"))?;
            args.insert(k.to_string(), to_json(v)?);
        }
    }
    let context = format!("task '{label}': ");
    let ignore_errors = flag(yaml, "ignore_errors")?;
    let timeout = timeout(yaml, &context)?;
    let async_val = seconds(yaml, "async", &context)?.unwrap_or(0);
    let poll = seconds(yaml, "poll", &context)?;
    let throttle = throttle(yaml, &context)?;
    let debugger = debugger(yaml, &context)?;
    let vars = mapping(yaml, "vars", &context)?;
    let when = conditions(yaml, "when", &context)?;
    let changed_when = conditions(yaml, "changed_when", &context)?;
    let failed_when = conditions(yaml, "failed_when", &context)?;
    let register = match field(yaml, "register") {
        None => None,
        Some(Yaml::Value(Scalar::String(s))) => Some(s.to_string()),
        Some(_) => bail!("task '{label}': 'register' must be a variable name"),
    };
    // Every loop keyword this release runs: `loop`, `with_items`, and each `with_<lookup>` whose
    // row says it runs. The others were parked above for the pre-flight.
    let mut loops = map.iter().filter_map(|(key, value)| {
        let key = key.as_str()?;
        let runs = key == "loop"
            || (key.starts_with("with_")
                && task_keyword(key).is_some_and(|k| k.support != Support::Preflight));
        runs.then_some((key, value))
    });
    let (loop_items, with_items, loop_with) = match (loops.next(), loops.next()) {
        (Some((first, _)), Some((second, _))) => {
            bail!("task '{label}': '{first}' and '{second}' cannot both be given")
        }
        (Some((key, v)), None) => (
            Some(to_json(v).with_context(|| format!("task '{label}': {key}"))?),
            key == "with_items",
            key.strip_prefix("with_")
                .filter(|lookup| *lookup != "items")
                .map(str::to_string),
        ),
        (None, _) => (None, false, None),
    };
    let (r#become, become_user) = escalation(yaml, &context)?;
    let (delegate_facts, mut delegate_to) = delegation(yaml, &context)?;
    // `local_action` runs on the controller whatever `delegate_to` says: measured, the line reads
    // `h1 -> localhost` with both written.
    if local {
        delegate_to = Some("localhost".to_string());
    }
    let mut value_origins = std::collections::BTreeMap::new();
    let var = field(value, "var").or_else(|| field(yaml, "args").and_then(|a| field(a, "var")));
    for (key, node) in [
        ("until", field(yaml, "until")),
        ("retries", field(yaml, "retries")),
        ("delay", field(yaml, "delay")),
        ("notify", field(yaml, "notify")),
        ("var", var),
    ] {
        if let Some(origin) = node.and_then(value_origin) {
            value_origins.insert(key.to_string(), origin);
        }
    }
    if let Some(origin) = key_origin(source.unwrap_or(value)) {
        value_origins.insert("action".to_string(), origin);
    }
    for node in [Some(source.unwrap_or(value)), field(yaml, "args")]
        .into_iter()
        .flatten()
    {
        if let Yaml::Mapping(map) = node {
            for (k, v) in map {
                if let (Some(k), Some(origin)) = (k.as_str(), value_origin(v)) {
                    value_origins.insert(format!("args.{k}"), origin);
                }
            }
        }
    }
    Ok(PlayTask {
        named: name.is_some(),
        name: name.unwrap_or_else(|| module.clone()),
        module,
        args,
        tags: tags(yaml, &context)?,
        ignore_errors,
        timeout,
        async_val,
        poll,
        throttle,
        debugger,
        vars,
        when,
        loop_items,
        with_items,
        loop_with,
        loop_var,
        loop_label,
        index_var,
        loop_extended,
        loop_allitems,
        loop_pause,
        break_when,
        register,
        changed_when,
        failed_when,
        r#become,
        become_user,
        connect: connect_keywords(yaml, &context)?,
        notify: names(yaml, "notify", &context)?,
        until: conditions(yaml, "until", &context)?,
        retries: field(yaml, "retries").map(to_json).transpose()?,
        delay: field(yaml, "delay").map(to_json).transpose()?,
        no_log: boolean(yaml, "no_log")?,
        environment: environment(yaml, &context)?,
        module_defaults: module_defaults(yaml, &context)?,
        collections: collections(yaml, &context)?,
        check_mode: flag(yaml, "check_mode")?,
        diff: flag(yaml, "diff")?,
        run_once: boolean(yaml, "run_once")?,
        any_errors_fatal: boolean(yaml, "any_errors_fatal")?,
        ignore_unreachable: boolean(yaml, "ignore_unreachable")?,
        delegate_facts,
        delegate_to,
        unsupported,
        origin: task_origin(map),
        value_origins,
    })
}

/// `tags`, wherever they are written: on a play, on a block, on a role entry or on a task.
///
/// Read the way the reference reads them, measured on ansible-core 2.19.12: a string is split
/// on commas and each piece trimmed, so `tags: a, b` is two tags; a list is taken entry by
/// entry, and a nested list is flattened. Anything else is refused in the reference's own
/// words. The result is sorted and deduplicated, because every use of it is a set: the two
/// listings sort what they print, and the selection asks whether a name is in the list.
fn tags(yaml: &Yaml, context: &str) -> anyhow::Result<Vec<String>> {
    let mut out = Vec::new();
    collect_tags(field(yaml, "tags"), context, &mut out)?;
    out.sort_unstable();
    out.dedup();
    Ok(out)
}

fn collect_tags(node: Option<&Yaml>, context: &str, out: &mut Vec<String>) -> anyhow::Result<()> {
    match node {
        None | Some(Yaml::Value(Scalar::Null)) => Ok(()),
        Some(Yaml::Value(Scalar::String(s))) => {
            out.extend(
                s.split(',')
                    .map(str::trim)
                    .filter(|t| !t.is_empty())
                    .map(str::to_string),
            );
            Ok(())
        }
        // A bare number or boolean is a tag too: the reference templates the value and keeps
        // whatever comes back, so `tags: 1` names the tag `1` rather than refusing the load.
        Some(Yaml::Value(Scalar::Integer(i))) => {
            out.push(i.to_string());
            Ok(())
        }
        Some(Yaml::Value(Scalar::Boolean(b))) => {
            out.push(if *b { "True" } else { "False" }.to_string());
            Ok(())
        }
        Some(Yaml::Sequence(items)) => {
            for item in items {
                collect_tags(Some(item), context, out)?;
            }
            Ok(())
        }
        Some(_) => bail!("{context}tags must be specified as a list"),
    }
}

/// `timeout`, on a task or on a block.
fn timeout(yaml: &Yaml, context: &str) -> anyhow::Result<Option<u64>> {
    seconds(yaml, "timeout", context)
}

/// A keyword counting seconds: `timeout`, `async`, `poll`.
fn seconds(yaml: &Yaml, key: &str, context: &str) -> anyhow::Result<Option<u64>> {
    match field(yaml, key) {
        None | Some(Yaml::Value(Scalar::Null)) => Ok(None),
        Some(Yaml::Value(Scalar::Integer(i))) if *i >= 0 => Ok(Some(*i as u64)),
        Some(other) => {
            bail!("{context}'{key}' must be a non-negative integer, found {other:?}")
        }
    }
}

/// `throttle`, wherever it is written. A negative number is no limit, as it is in the reference.
fn throttle(yaml: &Yaml, context: &str) -> anyhow::Result<Option<u64>> {
    match field(yaml, "throttle") {
        None | Some(Yaml::Value(Scalar::Null)) => Ok(None),
        Some(Yaml::Value(Scalar::Integer(i))) => Ok(Some((*i).max(0) as u64)),
        // Rendered by the reference when the task runs; this release reads a number only.
        Some(Yaml::Value(Scalar::String(_))) => {
            bail!("{context}'throttle' written as a string or a template is not supported yet")
        }
        Some(other) => bail!("{context}'throttle' must be an integer, found {other:?}"),
    }
}

/// What a `debugger` keyword says, which is when a result opens the debugger prompt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Debugger {
    Always,
    Never,
    OnFailed,
    OnUnreachable,
    OnSkipped,
}

/// `debugger`, wherever it is written, with the reference's own words for a value it does not
/// know.
fn debugger(yaml: &Yaml, context: &str) -> anyhow::Result<Option<Debugger>> {
    match field(yaml, "debugger") {
        None | Some(Yaml::Value(Scalar::Null)) => Ok(None),
        Some(Yaml::Value(Scalar::String(s))) => Ok(Some(match &**s {
            "always" => Debugger::Always,
            "never" => Debugger::Never,
            "on_failed" => Debugger::OnFailed,
            "on_unreachable" => Debugger::OnUnreachable,
            "on_skipped" => Debugger::OnSkipped,
            other => bail!(
                "{context}Error processing keyword 'debugger': '{other}' is not a valid value for debugger. Must be one of never, on_unreachable, on_failed, always, on_skipped"
            ),
        })),
        Some(other) => bail!("{context}'debugger' must be a name, found {other:?}"),
    }
}

/// A keyword whose value has to be a mapping, kept raw: `vars`, on a task or on a block.
fn mapping(yaml: &Yaml, key: &str, context: &str) -> anyhow::Result<Map<String, Value>> {
    match field(yaml, key) {
        None | Some(Yaml::Value(Scalar::Null)) => Ok(Map::new()),
        Some(v) => match to_json(v).with_context(|| format!("{context}'{key}'"))? {
            Value::Object(map) => Ok(map),
            _ => bail!("{context}'{key}' must be a mapping"),
        },
    }
}

/// `notify` and `listen`: one name or a list of them, kept in the order they were written - the
/// order a `notify` is written in decides nothing (handlers run in definition order, measured),
/// but keeping it is what lets a refusal name the first one the playbook wrote.
fn names(yaml: &Yaml, key: &str, context: &str) -> anyhow::Result<Vec<String>> {
    one_or_many(yaml, key, |node| {
        node.as_str()
            .map(str::to_string)
            .ok_or_else(|| anyhow!("{context}'{key}' must be a name or a list of names"))
    })
}

/// One value or a list of them, kept in the order they were written. An absent or null field is
/// an empty list, which is what the reference does with `notify:` written with nothing under it.
fn one_or_many<T>(
    yaml: &Yaml,
    key: &str,
    one: impl Fn(&Yaml) -> anyhow::Result<T>,
) -> anyhow::Result<Vec<T>> {
    match field(yaml, key) {
        None | Some(Yaml::Value(Scalar::Null)) => Ok(Vec::new()),
        Some(Yaml::Sequence(items)) => items.iter().map(one).collect(),
        Some(node) => Ok(vec![one(node)?]),
    }
}

/// A play's `handlers:` list, in the order it was written, which is the order they run in.
///
/// A block written in a handlers list is refused rather than flattened: the reference runs one,
/// and this release has nowhere to put a block's `rescue` in a flush. Refusing it by name beats
/// reading the block and running its tasks under rules nobody wrote.
fn parse_handlers(node: Option<&Yaml>) -> anyhow::Result<Vec<HandlerTask>> {
    let Some(node) = node else {
        return Ok(Vec::new());
    };
    let items = match node {
        Yaml::Sequence(items) => items,
        Yaml::Value(Scalar::Null) => return Ok(Vec::new()),
        _ => bail!("'handlers' must be a list"),
    };
    let mut handlers = Vec::new();
    for (i, item) in items.iter().enumerate() {
        let context = format!("handler {}: ", i + 1);
        let map = item
            .as_mapping()
            .ok_or_else(|| anyhow!("{context}a handler must be a mapping"))?;
        if map
            .keys()
            .any(|k| k.as_str().is_some_and(|k| BLOCK_SECTIONS.contains(&k)))
        {
            bail!("{context}a block in a handlers list is not supported yet");
        }
        let listen = names(item, "listen", &context)?;
        handlers.push(HandlerTask {
            task: parse_task(item, true).context(context)?,
            listen,
        });
    }
    Ok(handlers)
}

/// `when`, `changed_when`, `failed_when`: one expression or a list of them. A YAML boolean is
/// spelled back as Python would (`True`/`False`) so the expression evaluator reads it.
fn conditions(yaml: &Yaml, key: &str, context: &str) -> anyhow::Result<Vec<String>> {
    one_or_many(yaml, key, |node| condition(node, key, context))
}

fn condition(node: &Yaml, key: &str, context: &str) -> anyhow::Result<String> {
    match node {
        Yaml::Value(Scalar::String(s)) => Ok(s.to_string()),
        Yaml::Value(Scalar::Boolean(b)) => Ok(if *b { "True" } else { "False" }.to_string()),
        other => bail!(
            "{context}'{key}' must be an expression or a list of expressions, found {other:?}"
        ),
    }
}

fn module_args(module: &str, value: &Yaml) -> anyhow::Result<Map<String, Value>> {
    let mut args = Map::new();
    match value {
        // A `set_fact` key YAML reads as a number, a boolean or null keeps that type into the
        // name check, as the reference's does: `{1: x}` fails with `Invalid variable name '1' of
        // type 'int'.`, `{true: x}` with `'True' of type 'bool'`. Written as the template of
        // that value, the key renders back to it where the keys are rendered.
        Yaml::Mapping(entries) if short_name(module) == "set_fact" => {
            for (key, value) in entries {
                let key = match key {
                    Yaml::Value(Scalar::Integer(i)) => format!("{{{{ {i} }}}}"),
                    Yaml::Value(Scalar::FloatingPoint(f)) => {
                        format!("{{{{ {:?} }}}}", f.into_inner())
                    }
                    Yaml::Value(Scalar::Boolean(true)) => "{{ True }}".to_string(),
                    Yaml::Value(Scalar::Boolean(false)) => "{{ False }}".to_string(),
                    Yaml::Value(Scalar::Null) => "{{ None }}".to_string(),
                    other => other
                        .as_str()
                        .ok_or_else(|| anyhow::anyhow!("mapping keys must be strings"))?
                        .to_string(),
                };
                args.insert(key, to_json(value)?);
            }
        }
        Yaml::Mapping(_) => {
            if let Value::Object(map) = to_json(value)? {
                args = map;
            }
        }
        // `script`'s line is the reference's `parse_kv` with `check_raw`: its options come out,
        // the rest is the script and its arguments.
        Yaml::Value(Scalar::String(s))
            if crate::action_plugins::kind(module)
                == Some(crate::action_plugins::Kind::Bridged("script")) =>
        {
            let (options, raw) = crate::splitter::parse_kv_raw(s)?;
            for (k, v) in options {
                args.insert(k, Value::String(v));
            }
            if let Some(raw) = raw {
                args.insert("_raw_params".into(), Value::String(raw));
            }
        }
        Yaml::Value(Scalar::String(s)) if is_free_form(module) => {
            args.insert("_raw_params".into(), Value::String(s.to_string()));
        }
        // A collection's module, or a name a `library/` may hold, read as the reference's
        // `parse_kv` reads it: a word without `=` is kept in `_raw_params`, and a string that
        // does not split is kept whole, for the pre-flight to refuse once the name resolved.
        // Refused here instead, a role's file for another platform - read only to build the
        // union - would refuse a run no host of which reaches it.
        Yaml::Value(Scalar::String(s)) if crate::python::is_resolved_name(module) => {
            let (options, raw) =
                crate::splitter::parse_kv(s).unwrap_or((Vec::new(), Some(s.to_string())));
            for (k, v) in options {
                args.insert(k, Value::String(v));
            }
            if let Some(raw) = raw {
                args.insert("_raw_params".into(), Value::String(raw));
            }
        }
        // A builtin's: a word that is not `key=value` is kept the same way and fails the task
        // when it runs, unless the action takes raw parameters. Only a string `parse_kv` cannot
        // split stops the load, as it stops the reference's (`failed at splitting arguments`).
        Yaml::Value(Scalar::String(s)) => {
            let (options, raw) = crate::splitter::parse_kv(s)?;
            for (k, v) in options {
                args.insert(k, Value::String(v));
            }
            if let Some(raw) = raw {
                args.insert("_raw_params".into(), Value::String(raw));
            }
        }
        Yaml::Value(Scalar::Null) => {}
        // The class names are the reference's own words for what a task's argument was.
        Yaml::Value(Scalar::Boolean(_)) => {
            bail!("unexpected parameter type in action: <class 'bool'>")
        }
        Yaml::Value(Scalar::Integer(_)) => bail!(
            "unexpected parameter type in action: <class 'ansible.module_utils._internal._datatag._AnsibleTaggedInt'>"
        ),
        Yaml::Value(Scalar::FloatingPoint(_)) => bail!(
            "unexpected parameter type in action: <class 'ansible.module_utils._internal._datatag._AnsibleTaggedFloat'>"
        ),
        Yaml::Sequence(_) => bail!(
            "unexpected parameter type in action: <class 'ansible.module_utils._internal._datatag._AnsibleTaggedList'>"
        ),
        other => bail!("module arguments must be a mapping or a string, found {other:?}"),
    }
    Ok(args)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;

    /// The tasks of a play, for the tests that wrote none of their own blocks. A block here is
    /// the test's mistake, not the loader's, so it says so rather than being skipped.
    fn tasks(play: &Play) -> Vec<&PlayTask> {
        play.tasks
            .iter()
            .map(|item| match item {
                TaskOrBlock::Task(task) => task,
                TaskOrBlock::Block(_) => panic!("this playbook has no blocks"),
            })
            .collect()
    }

    /// The first task of the first play.
    fn first(pb: &Playbook) -> &PlayTask {
        tasks(&pb.plays[0])[0]
    }

    /// A task keeps where its mapping starts, as ansible-core's `Origin` gives it: measured,
    /// `- name: plain fail` on line 5 is `p.yml:5:7`.
    #[test]
    fn a_task_keeps_the_line_and_column_it_was_written_at() {
        let pb = parse(
            "- hosts: h1\n  gather_facts: false\n  tasks:\n    - debug: msg=hello\n    - name: plain fail\n      command: \"false\"\n",
            "p.yml",
        )
        .unwrap();
        let origins: Vec<(usize, usize)> = tasks(&pb.plays[0])
            .iter()
            .map(|t| t.origin.as_ref().map(|o| (o.line, o.col)).unwrap())
            .collect();
        assert_eq!(origins, [(4, 7), (5, 7)]);
        let path = tasks(&pb.plays[0])[0].origin.as_ref().unwrap().path.clone();
        assert!(
            path.ends_with("p.yml") && Path::new(&*path).is_absolute(),
            "{path}"
        );
    }

    const SAMPLE: &str = r"
- name: Smoke test
  hosts: web,db
  gather_facts: false
  tasks:
    - name: Say hello
      command: echo hello
    - shell: echo $HOME
      ignore_errors: yes
    - name: Structured args
      ansible.builtin.command:
        cmd: ls -l
        chdir: /tmp
    - name: Key value form
      raw: uptime
    - name: Args block
      command:
        argv: [true]
      args:
        chdir: /
";

    #[test]
    fn plays_and_tasks_are_loaded() {
        let pb = parse(SAMPLE, "site.yml").unwrap();
        assert_eq!(pb.plays.len(), 1);
        let play = &pb.plays[0];
        assert_eq!(play.name, "Smoke test");
        assert_eq!(play.hosts, "web,db");
        assert!(!play.gather_facts);
        let all = tasks(play);
        assert_eq!(all.len(), 5);

        let t = all[0];
        assert_eq!(
            (t.name.as_str(), t.module.as_str()),
            ("Say hello", "command")
        );
        assert_eq!(t.args["_raw_params"], "echo hello");
        assert!(!t.ignores_errors());

        let t = all[1];
        assert_eq!(t.name, "shell", "unnamed tasks take the module name");
        assert!(t.ignores_errors());

        let t = all[2];
        assert_eq!(t.module, "ansible.builtin.command");
        assert_eq!(t.args["cmd"], "ls -l");
        assert_eq!(t.args["chdir"], "/tmp");

        let t = all[4];
        assert_eq!(t.args["argv"], serde_json::json!([true]));
        assert_eq!(t.args["chdir"], "/");
    }

    #[test]
    fn hosts_lists_defaults_and_escalation_keywords_are_read() {
        let pb = parse("- hosts: [web, db]\n  tasks: []\n", "x.yml").unwrap();
        assert_eq!(pb.plays[0].hosts, "web,db");
        assert_eq!(pb.plays[0].name, "web,db", "unnamed plays take the pattern");
        assert!(pb.plays[0].gather_facts);
        let pb = parse(
            "- hosts: all
  become: yes
  become_user: deploy
  become_method: sudo
  strategy: free
  tasks:
    - command: id
      become: 'false'
      become_user: postgres
",
            "x.yml",
        )
        .unwrap();
        let play = &pb.plays[0];
        assert_eq!(play.r#become, Some(true), "'yes' is a boolean to Ansible");
        assert_eq!(play.become_user.as_deref(), Some("deploy"));
        assert_eq!(play.strategy.as_deref(), Some("free"));
        let t = tasks(play)[0];
        assert_eq!(t.r#become, Some(false), "a quoted spelling still reads");
        assert_eq!(t.become_user.as_deref(), Some("postgres"));
        let bare = parse(
            "- hosts: all
  tasks:
    - command: id
",
            "x.yml",
        )
        .unwrap();
        assert_eq!(
            (bare.plays[0].r#become, first(&bare).r#become.is_none()),
            (None, true),
            "silence at both levels leaves the decision to the variables and the defaults"
        );
    }

    /// Load refusals: the reference's own words where it has them, the engine's where it does not.
    #[rstest]
    #[case::play_attribute(
        "- hosts: all\n  nosuchplaykeyword: 1\n  tasks: []\n",
        &["'nosuchplaykeyword' is not a valid attribute for a Play"]
    )]
    #[case::become_method_play_doas(
        "- hosts: all\n  become: true\n  become_method: doas\n  tasks: []\n",
        &["doas", "not supported yet"]
    )]
    #[case::become_method_play_runas(
        "- hosts: all\n  become: true\n  become_method: runas\n  tasks: []\n",
        &["runas", "not supported yet"]
    )]
    #[case::become_method_task_doas(
        "- hosts: all\n  tasks:\n    - name: T\n      command: id\n      become_method: doas\n",
        &["doas", "T"]
    )]
    #[case::become_method_task_runas(
        "- hosts: all\n  tasks:\n    - name: T\n      command: id\n      become_method: runas\n",
        &["runas", "T"]
    )]
    #[case::no_module("- hosts: all\n  tasks:\n    - name: Nothing\n", &["no module"])]
    #[case::two_modules(
        "- hosts: all\n  tasks:\n    - command: a\n      shell: b\n",
        &["conflicting action statements: command, shell"]
    )]
    #[case::not_a_list("hosts: all\n", &[])]
    #[case::empty("", &[])]
    #[case::unbalanced_quote("- hosts: all\n  tasks:\n    - ping: \"a='open\"\n", &[])]
    #[case::when_scalar(
        "- hosts: all\n  tasks:\n    - command: \"true\"\n      when: 3\n",
        &["when"]
    )]
    #[case::loop_and_with_items(
        "- hosts: all\n  tasks:\n    - debug:\n      loop: [1]\n      with_items: [2]\n",
        &["loop"]
    )]
    #[case::two_with_loops(
        "- hosts: all\n  tasks:\n    - debug:\n      with_fileglob: ['*']\n      with_first_found: [a]\n",
        &["'with_fileglob' and 'with_first_found' cannot both be given"]
    )]
    #[case::hosts_int(
        "- hosts: [web, 3]\n  tasks: []\n",
        &["Hosts list contains an invalid host value: '3'"]
    )]
    #[case::hosts_float(
        "- hosts: [web, 3.5]\n  tasks: []\n",
        &["Hosts list contains an invalid host value: '3.5'"]
    )]
    #[case::hosts_sequence(
        "- hosts: [web, [1, 2]]\n  tasks: []\n",
        &["Hosts list contains an invalid host value: '[1, 2]'"]
    )]
    #[case::hosts_mapping(
        "- hosts: [web, {a: 1}]\n  tasks: []\n",
        &["Hosts list contains an invalid host value: '{'a': 1}'"]
    )]
    #[case::loop_control_unknown(
        "- hosts: all\n  tasks:\n    - debug:\n        msg: x\n      loop: [a]\n      loop_control:\n        nosuch: 1\n",
        &["'nosuch' is not a valid attribute for a LoopControl"]
    )]
    #[case::gather_facts_word(
        "- hosts: all\n  gather_facts: maybe\n  tasks:\n    - command: echo hi\n",
        &["Error processing keyword 'gather_facts': The value 'maybe' could not be converted to 'bool'."]
    )]
    #[case::gather_facts_two(
        "- hosts: all\n  gather_facts: 2\n  tasks:\n    - command: echo hi\n",
        &["The value 2 could not be converted to 'bool'."]
    )]
    #[case::ignore_errors_word(
        "- hosts: all\n  tasks:\n    - command: echo hi\n      ignore_errors: maybe\n",
        &["Error processing keyword 'ignore_errors': The value 'maybe' could not be converted to 'bool'."]
    )]
    #[case::become_word(
        "- hosts: all\n  tasks:\n    - command: echo hi\n      become: maybe\n",
        &["Error processing keyword 'become': The value 'maybe' could not be converted to 'bool'."]
    )]
    #[case::become_float(
        "- hosts: all\n  tasks:\n    - command: echo hi\n      become: 1.5\n",
        &["Error processing keyword 'become': The value 1.5 could not be converted to 'bool'."]
    )]
    #[case::become_sequence(
        "- hosts: all\n  tasks:\n    - command: echo hi\n      become: [1, 2]\n",
        &["Error processing keyword 'become': The value [1, 2] could not be converted to 'bool'."]
    )]
    #[case::become_mapping(
        "- hosts: all\n  tasks:\n    - command: echo hi\n      become: {a: 1}\n",
        &["Error processing keyword 'become': The value {'a': 1} could not be converted to 'bool'."]
    )]
    #[case::block_loop(
        "- hosts: all\n  tasks:\n    - block:\n        - command: echo hi\n      loop: x\n",
        &["'loop' is not a valid attribute for a Block"]
    )]
    #[case::block_register(
        "- hosts: all\n  tasks:\n    - block:\n        - command: echo hi\n      register: x\n",
        &["'register' is not a valid attribute for a Block"]
    )]
    #[case::block_until(
        "- hosts: all\n  tasks:\n    - block:\n        - command: echo hi\n      until: x\n",
        &["'until' is not a valid attribute for a Block"]
    )]
    #[case::block_module(
        "- hosts: all\n  tasks:\n    - block:\n        - command: echo hi\n      debug: msg=x\n",
        &["'debug' is not a valid attribute for a Block"]
    )]
    #[case::rescue_without_block(
        "- hosts: all\n  tasks:\n    - rescue:\n        - command: echo hi\n",
        &["'rescue' keyword cannot be used without 'block'"]
    )]
    #[case::always_without_block(
        "- hosts: all\n  tasks:\n    - always:\n        - command: echo hi\n",
        &["'always' keyword cannot be used without 'block'"]
    )]
    #[case::debugger_value(
        "- hosts: all\n  debugger: bogus\n  tasks:\n    - command: echo hi\n",
        &["'bogus' is not a valid value for debugger. Must be one of never, on_unreachable, on_failed, always, on_skipped"]
    )]
    #[case::throttle_word(
        "- hosts: all\n  tasks:\n    - command: echo hi\n      throttle: abc\n",
        &["'throttle' written as a string or a template is not supported yet"]
    )]
    #[case::notify_element(
        "- hosts: all\n  tasks:\n    - name: t\n      command: \"true\"\n      notify: [3]\n",
        &["task 't': 'notify' must be a name or a list of names"]
    )]
    #[case::when_element(
        "- hosts: all\n  tasks:\n    - name: t\n      command: \"true\"\n      when: [3]\n",
        &["task 't': 'when' must be an expression or a list of expressions"]
    )]
    #[case::async_text(
        "- hosts: all\n  tasks:\n    - name: t\n      command: \"true\"\n      async: x\n",
        &["task 't': 'async' must be a non-negative integer"]
    )]
    #[case::poll_negative(
        "- hosts: all\n  tasks:\n    - name: t\n      command: \"true\"\n      poll: -1\n",
        &["task 't': 'poll' must be a non-negative integer"]
    )]
    fn the_load_refuses(#[case] text: &str, #[case] needles: &[&str]) {
        let err = parse(text, "x.yml").expect_err("the load must refuse");
        let got = format!("{err:#}");
        for needle in needles {
            assert!(got.contains(needle), "{needle:?} not in {got}");
        }
    }

    /// Every `loop_control` sub-key is read into its field, and a malformed one is refused by name.
    #[test]
    fn loop_control_sub_keys_are_read() {
        let task = |sub: &str| {
            format!(
                "- hosts: all
  tasks:
    - debug:
        msg: x
      loop: [a]
      loop_control:
        {sub}
"
            )
        };
        let pb = parse(
            &task(
                "loop_var: thing
        label: shown",
            ),
            "x.yml",
        )
        .unwrap();
        let t = first(&pb);
        assert_eq!(
            (t.loop_var.as_str(), t.loop_label.as_deref()),
            ("thing", Some("shown"))
        );
        assert!(t.unsupported.is_empty());
        let pb = parse(
            &task(
                "index_var: i
        extended: true
        extended_allitems: false
        pause: 0.5
        break_when: [a, b]",
            ),
            "x.yml",
        )
        .unwrap();
        let t = first(&pb);
        assert_eq!(t.index_var.as_deref(), Some("i"));
        assert_eq!(
            (&t.loop_extended, &t.loop_allitems),
            (
                &Some(serde_json::json!(true)),
                &Some(serde_json::json!(false))
            )
        );
        assert_eq!(t.loop_pause, Some(serde_json::json!(0.5)));
        assert_eq!(t.break_when, ["a", "b"]);
        assert!(t.unsupported.is_empty());
        for bad in ["index_var: [i]", "break_when: {a: b}"] {
            assert!(parse(&task(bad), "x.yml").is_err(), "{bad}");
        }
    }

    /// A module's argument string is read as the reference's `parse_kv` reads it: `key=value` words
    /// become options and the rest is kept in `_raw_params`.
    #[rstest]
    #[case::set_fact("set_fact: owner=root state=here", "set_fact", &[("owner", "root"), ("state", "here")])]
    #[case::free_form_alias("ansible.legacy.shell: echo a=b", "ansible.legacy.shell", &[("_raw_params", "echo a=b")])]
    #[case::unknown_builtin("ansible.builtin.nosuch: echo a", "ansible.builtin.nosuch", &[])]
    #[case::library_stray("my_library_module: a=1 stray", "my_library_module", &[("_raw_params", "stray"), ("a", "1")])]
    #[case::file("file: path=/tmp/x state=touch", "file", &[("path", "/tmp/x"), ("state", "touch")])]
    #[case::collection("community.general.ufw: rule=allow port=22", "community.general.ufw", &[("port", "22"), ("rule", "allow")])]
    #[case::windows_free_text("ansible.windows.win_shell: Get-Service foo", "ansible.windows.win_shell", &[("_raw_params", "Get-Service foo")])]
    #[case::template_words("ns.coll.mod: port={{ x }} {{ a b }}", "ns.coll.mod", &[("_raw_params", "{{ a b }}"), ("port", "{{ x }}")])]
    #[case::open_quote("ns.coll.mod: a='open", "ns.coll.mod", &[("_raw_params", "a='open")])]
    #[case::builtin_stray_word("ping: data=x foo", "ping", &[("_raw_params", "foo"), ("data", "x")])]
    #[case::jinja_block_kept_whole(
        "copy: dest={{ base ~ '/x' }} mode=0644 content=u\"v w\"",
        "copy",
        &[("content", "u\"v w\""), ("dest", "{{ base ~ '/x' }}"), ("mode", "0644")]
    )]
    fn raw_arguments_are_split_as_the_reference_splits_them(
        #[case] line: &str,
        #[case] module: &str,
        #[case] expect: &[(&str, &str)],
    ) {
        let pb = parse(&format!("- hosts: all\n  tasks:\n    - {line}\n"), "x.yml")
            .unwrap_or_else(|e| panic!("{line}: {e:#}"));
        let task = first(&pb);
        assert_eq!(task.module, module);
        let mut got: Vec<(&str, &str)> = task
            .args
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str().unwrap_or_default()))
            .collect();
        got.sort_unstable();
        assert_eq!(got, expect, "{line}");
    }

    /// `action:` and `local_action:` read as the module's own key would, in every spelling the
    /// reference accepts; `local_action` runs on `localhost` whatever `delegate_to` says.
    #[rstest]
    #[case::string("action: command hostname", "command", &[("_raw_params", "hostname")], None)]
    #[case::string_options("action: copy src=a dest=b", "copy", &[("dest", "b"), ("src", "a")], None)]
    #[case::mapping("action: {module: command, cmd: hostname}", "command", &[("cmd", "hostname")], None)]
    #[case::mapping_args("action: {module: command, args: {cmd: hostname}}", "command", &[("cmd", "hostname")], None)]
    #[case::args_beside("action: command\n      args: {cmd: hostname}", "command", &[("cmd", "hostname")], None)]
    #[case::local("local_action: command hostname", "command", &[("_raw_params", "hostname")], Some("localhost"))]
    #[case::local_mapping("local_action: {module: command, cmd: hostname}", "command", &[("cmd", "hostname")], Some("localhost"))]
    #[case::local_beats_delegate("local_action: command hostname\n      delegate_to: h1", "command", &[("_raw_params", "hostname")], Some("localhost"))]
    fn an_action_statement_names_the_module_in_its_value(
        #[case] line: &str,
        #[case] module: &str,
        #[case] expect: &[(&str, &str)],
        #[case] delegate: Option<&str>,
    ) {
        let pb = parse(&format!("- hosts: all\n  tasks:\n    - {line}\n"), "x.yml")
            .unwrap_or_else(|e| panic!("{line}: {e:#}"));
        let task = first(&pb);
        assert_eq!(
            (task.module.as_str(), task.delegate_to.as_deref()),
            (module, delegate)
        );
        assert!(task.unsupported.is_empty());
        let mut got: Vec<(&str, &str)> = task
            .args
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str().unwrap_or_default()))
            .collect();
        got.sort_unstable();
        assert_eq!(got, expect, "{line}");
    }

    /// The statements the reference refuses, in its words.
    #[rstest]
    #[case::module_equals(
        "action: module=command cmd=hostname",
        "Cannot resolve 'module=command' to an action or module."
    )]
    #[case::no_module("action: {cmd: hostname}", "no module/action detected in task.")]
    #[case::empty("action: ''", "no module/action detected in task.")]
    #[case::two_modules(
        "local_action: command hostname\n      command: hostname",
        "conflicting action statements: command, command"
    )]
    #[case::both(
        "action: command a\n      local_action: command b",
        "action and local_action are mutually exclusive"
    )]
    fn an_action_statement_the_reference_refuses_is_refused(#[case] line: &str, #[case] why: &str) {
        let err =
            parse(&format!("- hosts: all\n  tasks:\n    - {line}\n"), "x.yml").expect_err(line);
        assert!(format!("{err:#}").contains(why), "{line}: {err:#}");
    }

    const KEYWORDS: &str = r#"
- name: Keywords
  hosts: all
  gather_facts: false
  vars:
    port: 80
    greeting: "hello {{ name }}"
  vars_files:
    - vars/common.yml
    - "vars/{{ env }}.yml"
  tasks:
    - name: Conditional
      command: echo {{ port }}
      when: port > 1
      register: out
      vars:
        local: 1
      timeout: 5
    - name: Several conditions
      command: "true"
      when:
        - out is defined
        - out.rc == 0
      changed_when: false
      failed_when:
        - out.rc != 0
        - "'oops' in out.stdout"
    - name: Looping
      debug:
        msg: "{{ item }}"
      loop: "{{ ['a', 'b'] }}"
    - name: Legacy loop with control
      debug:
        msg: "{{ server.name }}"
      with_items:
        - {name: a}
        - {name: b}
      loop_control:
        loop_var: server
        label: "{{ server.name }}"
    - name: Facts
      set_fact:
        computed: "{{ port + 1 }}"
"#;

    #[test]
    fn play_and_task_keywords_are_read_raw() {
        let pb = parse(KEYWORDS, "k.yml").unwrap();
        let play = &pb.plays[0];
        assert_eq!(play.vars["port"], serde_json::json!(80));
        assert_eq!(play.vars["greeting"], serde_json::json!("hello {{ name }}"));
        assert_eq!(play.vars_files, ["vars/common.yml", "vars/{{ env }}.yml"]);
        let t = tasks(play);
        assert_eq!(t[0].when, ["port > 1"]);
        assert_eq!(t[0].register.as_deref(), Some("out"));
        assert_eq!(t[0].vars["local"], serde_json::json!(1));
        assert_eq!(t[0].timeout, Some(5));
        assert_eq!(
            t[0].args["_raw_params"], "echo {{ port }}",
            "arguments stay untemplated here"
        );

        assert_eq!(t[1].when, ["out is defined", "out.rc == 0"]);
        assert_eq!(t[1].changed_when, ["False"]);
        assert_eq!(t[1].failed_when, ["out.rc != 0", "'oops' in out.stdout"]);

        assert_eq!(t[2].loop_items, Some(serde_json::json!("{{ ['a', 'b'] }}")));
        assert_eq!(t[2].loop_var, "item");
        assert!(t[2].loop_label.is_none());

        assert_eq!(
            t[3].loop_items,
            Some(serde_json::json!([{"name": "a"}, {"name": "b"}]))
        );
        assert_eq!(t[3].loop_var, "server");
        assert_eq!(t[3].loop_label.as_deref(), Some("{{ server.name }}"));
        assert!(t[3].with_items && !t[2].with_items);

        assert_eq!(t[4].module, "set_fact");
        assert_eq!(t[4].args["computed"], "{{ port + 1 }}");
    }

    /// `when: false` and `1`/`0` are booleans to the reference; a template stays as written.
    #[test]
    fn booleans_are_read_the_way_the_reference_reads_them() {
        let pb = parse(
            "- hosts: all\n  gather_facts: 1\n  tasks:\n    - command: \"true\"\n      when: false\n      ignore_errors: 0\n",
            "x.yml",
        )
        .unwrap();
        assert!(pb.plays[0].gather_facts && !first(&pb).ignores_errors());
        assert_eq!(first(&pb).when, ["False"]);
        let pb = parse(
            "- hosts: all\n  tasks:\n    - command: \"false\"\n      ignore_errors: \"{{ ansible_check_mode }}\"\n    - block:\n        - command: echo hi\n      ignore_errors: \"{{ true }}\"\n",
            "x.yml",
        )
        .unwrap();
        let TaskOrBlock::Task(t) = &pb.plays[0].tasks[0] else {
            panic!("a task");
        };
        assert_eq!(
            t.ignore_errors,
            Some(Flag::Template("{{ ansible_check_mode }}".into()))
        );
        assert!(
            !t.ignores_errors(),
            "an unrendered template ignores nothing"
        );
        let TaskOrBlock::Block(b) = &pb.plays[0].tasks[1] else {
            panic!("a block");
        };
        assert_eq!(
            b.keywords.ignore_errors,
            Some(Flag::Template("{{ true }}".into()))
        );
    }

    /// A block takes its three sections and the keywords its tasks inherit, and nests; an empty
    /// block loads.
    #[test]
    fn a_block_takes_its_sections_and_its_inherited_keywords() {
        let pb = parse(
            "- hosts: all\n  tasks:\n    - name: Grouped\n      block:\n        - command: echo hi\n        - block:\n            - command: echo deeper\n      rescue:\n        - command: echo sorry\n      always:\n        - command: echo done\n      when: ready\n      become: true\n      ignore_errors: true\n      vars: {a: 1}\n    - block: []\n",
            "x.yml",
        )
        .unwrap();
        let TaskOrBlock::Block(b) = &pb.plays[0].tasks[0] else {
            panic!("a block");
        };
        assert_eq!(b.body.len(), 2);
        assert!(matches!(b.body[1], TaskOrBlock::Block(_)), "blocks nest");
        assert_eq!(b.rescue.len(), 1);
        assert_eq!(b.always.len(), 1);
        assert_eq!(b.keywords.name, "Grouped");
        assert_eq!(b.keywords.when, ["ready"]);
        assert_eq!(b.keywords.r#become, Some(true));
        assert_eq!(b.keywords.ignore_errors, Some(Flag::Fixed(true)));
        assert_eq!(b.keywords.vars["a"], serde_json::json!(1));
        assert!(
            b.keywords.module.is_empty(),
            "a block names no module of its own"
        );
        let TaskOrBlock::Block(empty) = &pb.plays[0].tasks[1] else {
            panic!("a block");
        };
        assert!(empty.body.is_empty());
    }

    /// Shapes the loader keeps for the pre-flight: a `set_fact` key
    /// typed by YAML, and a `meta` action whatever it names.
    #[test]
    fn what_the_loader_keeps_for_the_preflight() {
        let pb = parse(
            "- hosts: all\n  tasks:\n    - set_fact: {1: a, 1.5: b, 2.0: f, true: c, ~: d, k: e}\n",
            "x.yml",
        )
        .unwrap();
        let keys: Vec<&str> = first(&pb).args.keys().map(String::as_str).collect();
        assert_eq!(
            keys,
            [
                "{{ 1 }}",
                "{{ 1.5 }}",
                "{{ 2.0 }}",
                "{{ True }}",
                "{{ None }}",
                "k"
            ]
        );
        for action in ["noop", "end_play", "nosuchaction"] {
            let pb = parse(
                &format!("- hosts: all\n  tasks:\n    - meta: {action}\n"),
                "x.yml",
            )
            .unwrap_or_else(|e| panic!("{action}: {e:#}"));
            let t = first(&pb);
            assert_eq!(t.module, "meta");
            assert_eq!(t.args["_raw_params"], serde_json::json!(action));
        }
    }
}
