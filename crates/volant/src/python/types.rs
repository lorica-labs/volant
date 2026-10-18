// SPDX-License-Identifier: GPL-3.0-or-later

use std::collections::BTreeMap;

use serde_json::{Map, Value};
use volant_protocol::modules::short_name;

/// The helper itself, shipped in the binary and handed to the interpreter on its command line.
/// Nothing is written to disk for it, so no temporary file can be left behind by a run that
/// dies.
pub(crate) const HELPER: &str = include_str!("../python_helper.py");

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
    /// Whether the file built is ansible-core's own module. Measured on ansible-core 2.19.12, a
    /// `library/stat.py` is built as `ansible.legacy.stat` and a collection's module under
    /// `ansible_collections.`, neither of which the agent's `ansible.modules.<name>` lookup
    /// matches; this is the second barrier, for whatever a wrapper names a module next. A helper
    /// that says nothing is read as `false`, which sends the payload.
    pub core: bool,
}

/// Where `setup` gets its facts: `--facts`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, clap::ValueEnum)]
pub enum Facts {
    /// The native collector when the plays provably read only the keys it produces
    /// (`facts_read`), the Python module otherwise. Resolved to one of the two others before
    /// the union is armed; read as `Python` wherever it is not.
    #[default]
    Auto,
    Python,
    Native,
}

/// Which tasks of a run may be answered by an agent's native module instead of their payload.
///
/// The default is natives off: a union whose policy nobody set sends every task to Python, so a
/// lost assignment shows as no native ever answering rather than as natives answering a run
/// that switched them off.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Natives {
    /// `[volant] native_modules`, or `VOLANT_NATIVE_MODULES`.
    pub enabled: bool,
    pub facts: Facts,
}

impl Natives {
    /// Whether a task whose module was built as `facts` has to run its payload whatever native
    /// the agent has enabled.
    ///
    /// `setup` is recognised by the name it was built under, which is the name the agent looks a
    /// native up by: a collection's module redirected to `ansible.builtin.setup` is ansible-core's
    /// `setup` whatever the task called it.
    pub fn force_python(self, facts: &ModuleFacts) -> bool {
        if !self.enabled || !facts.core {
            return true;
        }
        facts.module_fqn == "ansible.modules.setup" && self.facts != Facts::Native
    }
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
    /// Keyed by [`payload_key`]: `ping` and `ansible.legacy.ping` are one entry, and
    /// `ansible.posix.sysctl` is its own, whatever else in the run is called `sysctl`.
    pub modules: BTreeMap<String, ModuleFacts>,
    /// A collection's module a role file named and the controller's ansible-core did not resolve
    /// to a module it can build, with the refusal the pre-flight gives a play naming it
    /// (`preflight::refusal_of`): kept out of the union, and said by the include that
    /// reaches it. A union may hold only these, with no zip and no module: nothing is sent then,
    /// since no task has a payload.
    pub refused: BTreeMap<String, String>,
    /// What the run's configuration lets a native module answer for, read into every payload.
    pub natives: Natives,
}

impl Union {
    /// The module's half of the payload a task naming `module` runs with, when the run built one.
    pub fn payload(&self, module: &str) -> Option<ModulePayload> {
        let facts = self.modules.get(payload_key(module))?;
        Some(ModulePayload {
            blob: self.hash.clone(),
            facts: facts.clone(),
            force_python: self.natives.force_python(facts),
        })
    }
}

/// The key a module's facts are filed under in [`Union::modules`], and looked up by.
///
/// A builtin written bare or as `ansible.legacy.` keeps its short name, so those two spellings of
/// `ping` are one entry: both search a playbook's or a role's `library/` first. `ansible.builtin.`
/// is kept whole, because ansible-core looks that name up among its own modules alone
/// (`_find_fq_plugin`): measured on 2.19.12 with a `library/ping.py`, `ping` and
/// `ansible.legacy.ping` run it and `ansible.builtin.ping` runs ansible-core's. Every other name
/// is kept whole too: two collections may each ship a `sysctl`, and a collection may ship a module
/// named like a builtin, so a key shortened to the last segment would hand a task the module of
/// whichever of them was built last.
pub fn payload_key(module: &str) -> &str {
    if volant_protocol::modules::is_builtin(module) && !module.starts_with("ansible.builtin.") {
        short_name(module)
    } else {
        module
    }
}

/// Whether only the controller's ansible-core can say what this name is: a collection's module,
/// or a name no builtin has (`my_module`, `ansible.legacy.my_module`), which a `library/` beside
/// the playbook, in a role, or on the configured path may hold. `ansible.builtin.nosuch` is not
/// one: it is a typo in a namespace ansible-core owns whole, and nothing can supply it.
pub fn is_resolved_name(module: &str) -> bool {
    !module.is_empty()
        && !module.starts_with("ansible.builtin.")
        && !volant_protocol::modules::is_builtin(module)
}

/// Tells the action plugin dispatch which of the names ansible-core resolved are collection
/// action plugins, which run in the helper rather than as a module.
pub(crate) fn note_action_plugins(resolved: &BTreeMap<String, Resolved>) {
    for (name, answer) in resolved {
        if let Resolved::ActionPlugin { fqcn, with_module } = answer
            && !crate::action_plugins::waits_for_later(name)
            && !crate::action_plugins::waits_for_later(fqcn)
        {
            crate::action_plugins::note_collection_action(name, fqcn, *with_module);
        }
    }
}

/// What the controller's ansible-core makes of one module name it was asked about.
///
/// Only the helper can answer: the collections are the controller's, installed where its
/// ansible-core looks for them, and nothing is fetched to find out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Resolved {
    /// A module it can build; `collection` is `None` for a builtin, else `(name, version)`, the
    /// version being `*` for a collection that carries no `MANIFEST.json`, as ansible-galaxy
    /// lists it.
    Module {
        fqcn: String,
        collection: Option<(String, String)>,
    },
    /// A name a collection serves through an action plugin, its own or one its `runtime.yml`
    /// routes it to: run in the helper as ansible-core's action plugin. `fqcn` is the plugin
    /// that runs (`runtime.yml`'s `action_plugin`, else the one of the module's name), and
    /// `with_module` says the module of the asked name builds, for a plugin that runs it.
    ActionPlugin { fqcn: String, with_module: bool },
    /// A name ansible-core knows and this release cannot run: a module its collection removed
    /// (a `runtime.yml` tombstone), one with no Python source (a `.ps1` module whose `.py` holds
    /// only its documentation), or one the helper failed to build. `reason` is ansible-core's
    /// own sentence.
    Unusable { reason: String },
    /// Nothing answers to it; `collection` names what would have to be installed, and is `None`
    /// when that collection is installed and has no such module, or the name names no
    /// collection at all.
    Missing { collection: Option<String> },
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
    /// Sent as [`volant_protocol::Task::force_python`].
    pub force_python: bool,
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
