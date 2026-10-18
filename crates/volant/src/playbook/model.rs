// SPDX-License-Identifier: GPL-3.0-or-later
//! The parsed shape of a playbook: plays, tasks, blocks and the connection keywords.

use std::path::PathBuf;

use serde_json::{Map, Value};
use volant_protocol::modules::short_name;

use crate::roles::RoleEntry;

use super::fields::Debugger;
use super::keywords::{Flag, META_ONCE, is_meta};
use super::origin::Origin;

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
    pub throttle: Option<Throttle>,
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

/// A `throttle` as written, and where its value stands in the file: the reference reads it when
/// the task runs, so a value that is not a number is an error of that moment.
#[derive(Debug, Clone, PartialEq)]
pub struct Throttle {
    pub value: Value,
    pub origin: Option<Origin>,
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
    /// `throttle`: the most hosts that run this task at the same time, as written. It is
    /// rendered and converted when the task runs, for each host.
    pub throttle: Option<Throttle>,
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
/// [`super::BECOME_METHODS`]. A task's own value wins over its block's and its play's, and a host
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
