// SPDX-License-Identifier: GPL-3.0-or-later
//! Loading a playbook or a task file from disk.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, anyhow, bail};
use saphyr::{Scalar, Yaml};
use serde_json::{Map, Value};

use crate::bridge::Bridge;
use crate::keywords::BLOCK_SECTIONS;
use crate::template::Templar;

use super::fields::parse_handlers;
use super::import::Import;
use super::model::{HandlerTask, Playbook, TaskOrBlock};
use super::origin::OriginScope;
use super::play::parse_play;
use super::task::{parse_block, parse_task};
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
    load_rendering(path, &Rendering::default())
}

/// [`load`], with the engine and the variables an `import_playbook` name renders against: the
/// run's helper, so a collection filter works there, and `-e`.
///
/// # Errors
/// As [`load`].
pub fn load_rendering(path: &Path, rendering: &Rendering<'_>) -> anyhow::Result<Playbook> {
    read(path, 0, rendering).map_err(|err| crate::stats::Refusal::or(4, err))
}

/// What the name of an `import_playbook` is rendered with.
pub struct Rendering<'a> {
    templar: Templar,
    vars: &'a Map<String, Value>,
}

impl<'a> Rendering<'a> {
    #[must_use]
    pub fn new(bridge: Option<Arc<Bridge>>, vars: &'a Map<String, Value>) -> Self {
        Self {
            templar: Templar::with_bridge(PathBuf::from("."), bridge),
            vars,
        }
    }
}

impl Rendering<'_> {
    /// Gives the helper the plugin directories beside `files` before any of their imports is
    /// named: measured on ansible-core 2.19.12, a filter of the playbook's `filter_plugins/`
    /// renders an `import_playbook` name.
    ///
    /// # Errors
    /// Why the running helper refused the directories.
    pub fn add_plugin_dirs(&self, files: &[PathBuf]) -> anyhow::Result<()> {
        match self.templar.bridge() {
            Some(bridge) => bridge.set_plugin_dirs(files.iter().map(|f| base_dir(f)).collect()),
            None => Ok(()),
        }
    }
}

impl Default for Rendering<'static> {
    fn default() -> Self {
        static NONE: std::sync::LazyLock<Map<String, Value>> = std::sync::LazyLock::new(Map::new);
        Self::new(None, &NONE)
    }
}

/// How deep `import_playbook` may nest before the run is refused. A file importing itself would
/// otherwise read on until the process ran out of stack, and a refusal naming the depth says
/// which file to look at.
const IMPORT_DEPTH: usize = 32;

fn read(path: &Path, depth: usize, rendering: &Rendering<'_>) -> anyhow::Result<Playbook> {
    // Exit 1 for a file that is not there, measured, and not the 4 a file that is there and
    // makes no sense gets. It carries its own code so the blanket [`load`] wraps the parse in
    // cannot take it.
    let text = crate::vault::read_to_string(path).map_err(|err| {
        // Measured on ansible-core 2.19.12 for a file that is not there.
        if err.kind() == std::io::ErrorKind::NotFound {
            crate::stats::Refusal::plain(
                1,
                format!("the playbook: {} could not be found", path.display()),
            )
        } else {
            crate::stats::Refusal::at(1, format!("reading playbook {}: {err}", path.display()))
        }
    })?;
    parse_at(
        &text,
        &path.display().to_string(),
        &base_dir(path),
        depth,
        rendering,
    )
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
    parse_at(
        text,
        source,
        &base_dir(Path::new(source)),
        0,
        &Rendering::default(),
    )
}

/// One playbook, with every `import_playbook` entry replaced in place by the plays of the file
/// it names.
///
/// In place, not appended: measured on ansible-core 2.19.12, a file importing `sub.yml`, then
/// declaring a play of its own, then importing `sub.yml` again runs the three plays in the order
/// they are written. Each imported play keeps the directory of the file it came from, so its
/// roles and its `vars_files` are looked for beside that file and not beside the playbook the
/// operator typed.
fn parse_at(
    text: &str,
    source: &str,
    dir: &Path,
    depth: usize,
    rendering: &Rendering<'_>,
) -> anyhow::Result<Playbook> {
    let (docs, origins) = crate::yaml::load_with_origins(text, source)?;
    let _origins = OriginScope::enter(source, origins);
    let Some(Yaml::Sequence(entries)) = docs.first() else {
        bail!("{source}: a playbook must be a list of plays");
    };
    let mut plays = Vec::new();
    for (i, entry) in entries.iter().enumerate() {
        match imported_playbook(entry, rendering)? {
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
                let import =
                    Import::read(entry).with_context(|| format!("{source}: play {}", i + 1))?;
                let mut imported = read(&path, depth + 1, rendering)
                    .with_context(|| format!("{source}: import_playbook {name}"))?
                    .plays;
                for play in &mut imported {
                    import.apply(play);
                }
                plays.extend(imported);
            }
            None => plays.push(
                rendered_play(entry, dir, rendering)
                    .with_context(|| format!("{source}: play {}", i + 1))?,
            ),
        }
    }
    Ok(Playbook { plays })
}

/// A play, with the names of its `roles:` entries rendered where they are templates.
///
/// Measured on ansible-core 2.19.12: `role: "{{ playbook_dir }}"` runs the role in the
/// playbook's own directory (`ansible-lockdown/UBUNTU24-CIS` applies itself that way). The
/// reference renders the name while it loads the play, against the play's variables, the extra
/// vars and `playbook_dir`; no host is known yet, so nothing of the inventory takes part.
fn rendered_play(
    entry: &Yaml,
    dir: &Path,
    rendering: &Rendering<'_>,
) -> anyhow::Result<super::model::Play> {
    let mut play = parse_play(entry, dir)?;
    if !play.roles.iter().any(|r| Templar::is_template(&r.name)) {
        return Ok(play);
    }
    let mut vars = play.vars.clone();
    vars.extend(rendering.vars.iter().map(|(k, v)| (k.clone(), v.clone())));
    vars.insert(
        "playbook_dir".into(),
        Value::String(dir.display().to_string()),
    );
    for role in play
        .roles
        .iter_mut()
        .filter(|r| Templar::is_template(&r.name))
    {
        match rendering.templar.render(&role.name, &vars) {
            Ok(Value::String(name)) => role.name = name,
            Ok(other) => bail!(
                "the role name {:?} renders to {other}, not a name",
                role.name
            ),
            Err(err) => bail!(
                "the role name {:?} cannot be rendered: {}",
                role.name,
                err.0
            ),
        }
    }
    Ok(play)
}

/// The file an `import_playbook` entry names, rendered.
///
/// The reference renders the value with the extra vars and nothing else - measured, `{{ 'sub'
/// }}.yml` and `{{ which }}.yml` with `-e which=sub.yml` work, and `{{ nosuchvar }}.yml` refuses
/// the load with `Error processing keyword 'import_playbook': 'nosuchvar' is undefined`, exit 4.
/// Nothing has an inventory yet at this point, so a name that depends on a host could not be
/// answered anyway.
fn imported_playbook(entry: &Yaml, rendering: &Rendering<'_>) -> anyhow::Result<Option<String>> {
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
    if !Templar::is_template(raw) {
        return Ok(Some(raw.to_string()));
    }
    let rendered = rendering
        .templar
        .render(raw, rendering.vars)
        .map_err(|err| anyhow!("Error processing keyword 'import_playbook': {}", err.0))?;
    match rendered {
        Value::String(name) => Ok(Some(name)),
        other => bail!("Error processing keyword 'import_playbook': {other} is not a file name"),
    }
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
        } else if let Some(located) = err.downcast_ref::<super::Located>() {
            anyhow::Error::new(TaskFileRefused(located.text(), true))
        } else {
            anyhow::Error::new(TaskFileRefused(format!("{err:#}"), false))
        }
    })
}

/// A task file the reference refuses while it loads it: a YAML error, a task it cannot parse.
/// Raised from a dynamic include it ends the run at exit 4 with the statement's banner and no
/// recap; measured on ansible-core 2.19.12, unlike [`NotATaskList`], which fails the host. The
/// flag says the text is an `[ERROR]` block's, printed without `ERROR!`.
#[derive(Debug)]
pub(crate) struct TaskFileRefused(pub String, pub bool);

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
pub(super) fn task_list(node: Option<&Yaml>, label: &str) -> anyhow::Result<Vec<TaskOrBlock>> {
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
