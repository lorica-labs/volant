---
title: Action plugins
description: How Volant runs copy, package, service, template and unarchive, which ansible-core runs through an action plugin on the controller.
---

Most modules ansible-core ships run as themselves: the controller sends a payload, the host runs it, the result comes back. A handful run differently: the reference itself decides what to send, sometimes after asking the host something first, and the module name on the task is only the entry point. `package` is one: what runs is `apt` or `dnf`, chosen after the host says which one it has. `copy` is another: whether anything travels to the host at all depends on a checksum the host reports back first.

Volant runs seven of these itself: `copy`, `dnf`, `fetch`, `package`, `service`, `template` and `unarchive`. Each one is a small state machine on the controller: asked for the next step, handed the result of the last one, until it has the task's own result. Every step it takes is an ordinary module of the run's union, sent alone over the link the task already has. The result a plugin ends with goes down the same road every other module result takes, so nothing a host answered reaches a playbook's variables by a way of its own.

Ten other names the reference also runs through an action plugin are not supported yet: Volant names them before the first connection, because sending their module alone would run something other than what the playbook asked for: `add_host`, `assemble`, `async_status`, `gather_facts`, `group_by`, `reboot`, `script`, `set_stats`, `uri`, `wait_for_connection`. See [Modules](/reference/modules/) for what this release runs natively, on the controller, and on the warm Python path.

## `copy`

A controller file, or a `content:` written straight into the destination, sent to the host only when it differs.

The plugin runs `stat` on the destination first, asking for a SHA-1 unless `force: false`, in which case it asks for none and leaves an existing destination untouched whatever it holds. It compares that sum with the one of the local bytes. A destination already holding the same bytes gets `file`, to set the attributes the task asked for; everything else gets the bytes staged on the host and a `copy` sub-task. Only that last case sends anything.

`remote_src: true` skips all of this: `copy` runs alone, with the task's own arguments, and `src` names a file already on the host.

Rejected before the host is asked anything, in the reference's own words:

- `src (or content) is required`
- `dest is required`
- `src and content are mutually exclusive`
- `can not use content with a dir as dest`
- `copying a directory is not supported yet: <src>` (the reference copies a directory recursively; this release does not yet).
- A `src` that names nothing on the controller, wrapped as `Unexpected AnsibleActionFail error: Could not find or access '<src>' ...`.
- A source larger than one frame is rejected by its size, before it is read.

## `template`

A template rendered on the controller, then handed to the same path `copy` uses.

The plugin finds the template under `templates/`, renders it once with the task's variables and its own delimiters, and copies the result the way `copy` copies a `content:`: `stat`, then `file` or a staged `copy`. Rendering happens exactly once; nothing renders the result a second time, so a value a managed host contributed lands in the file as text rather than as a template of its own.

Four variables are set for the render, on top of the task's own: `ansible_managed` (unless the task already set it), `template_path` (`src` as the task wrote it), `template_fullpath` (the path found on the controller) and `template_destpath` (`dest` as rendered, see below). `template_host`, `template_uid`, `template_run_date` and `template_mtime`, which the reference also sets, are not.

`trim_blocks` defaults on and `lstrip_blocks` defaults off, as in the reference, and the file's trailing newline is kept. `newline_sequence` takes `\n`, `\r` or `\r\n`, the escaped four-character spelling read the same as the literal one. The six delimiter options (`variable_start_string` and its five relatives) are not supported yet and are named rather than silently ignored, and so is an `output_encoding` other than UTF-8. A template that uses `{% include %}` or `{% import %}` fails: nothing here loads a second file mid-render.

Rejected before the host is asked anything:

- `'state' cannot be specified on a template`
- `src and dest are required`
- `newline_sequence needs to be one of: \n, \r or \r\n`
- `argument '<name>' is not supported yet on 'template'`, for a delimiter option
- `output_encoding '<encoding>' is not supported yet on 'template': only utf-8 is`
- A `src` that names nothing on the controller, in the reference's own words, with no `AnsibleActionFail` wrapper this time.
- A render bigger than one frame is rejected, naming both sizes, before anything is staged.

## `package`

The host's package manager names the module that actually runs.

The name comes from `use:`, unless it is `auto`; then from the host's `ansible_package_use` variable; then from `ansible_facts.pkg_mgr` of the host the module runs on; then from a `setup` filtered to that one fact, run again for every task and never kept. Whichever module the name picks is what runs, with `use` taken out of its arguments; a `setup` that fails ends the task with `Failed to fetch ansible_pkg_mgr to determine the package action backend: <msg>`, and a name that resolves to nothing this release can dispatch fails with `Could not find a matching action for the "<name>" package manager.` A name a host reports is never anything but a lookup key into this closed list: `setup`, `apt`, `dnf`, `dnf5`. The union carries all four backends rather than only the one a run turns out to need, because nothing is built after the facts are known: a plugin can only ever pick among what already travelled.

## `dnf`

The host's package manager again, this time picking between two module names instead of four.

The backend name comes from `use:`, or from `use_backend:` when `use` is absent; the two together are rejected: `parameters are mutually exclusive: ('use', 'use_backend')`. `auto` and `yum` read `ansible_facts.pkg_mgr` of the host the module runs on, gathered through a `setup` filtered to that one fact when it is not already known; any other name is taken as given. `yum`, `yum4` and `dnf4` all run the `dnf` module, `dnf5` runs `dnf5`, and a name that is still none of these fails with the reference's own two-line message, closing brace and all:

```text
Could not detect which major revision of dnf is in use, which is required to determine module backend.
You should manually specify use_backend to tell the module whether to use the dnf4 or dnf5 backend})
```

A failed `setup` ends the task with its own cause, `Failed to fetch ansible_pkg_mgr to determine the package action backend: <msg>`. When the task is not delegated, its result carries `ansible_facts.pkg_mgr` set to whatever the `setup` answered, the way the reference's own result does; a delegated task's result carries none, since a delegate's answer would otherwise land under the wrong host's facts.

## `fetch`

A file read from a host, written onto the controller under `dest` and nowhere else.

Without `become` the plugin runs `stat` on the source first and leaves an already-matching local file alone; under `become` it goes straight to `slurp`. The bytes always travel back through `slurp`, decoded strictly, never rendered and never put in a variable. The destination is `dest/<inventory_hostname>/<src>`, unless `flat: true` names `dest` itself directly, or, with a trailing slash, `dest/<basename of src>`.

Measured on ansible-core 2.19.12, a relative `src` climbing with `..` makes the reference write outside `dest` on the controller: a `src` of `../../../../../../../../tmp/x` without `flat` writes `/tmp/x`. Here every candidate path is built and normalised without touching the filesystem, and refused unless it sits under `dest`, before a directory is even created:

- `the fetched path <p> is outside '<dest>'`

This is checked twice: once on the `src` the playbook wrote, and again on the path the host names back, which does not have to match. A `dest` whose render read a managed host is refused before anything is asked, and so is a loop's `inventory_hostname` when a host set it, or one that is not a single path component:

- `the 'dest' of this task was named by a managed host, and a controller path a host chose is never written`
- `the inventory_hostname '<name>' is not one path component, and fetch files what it fetches under it`

A symbolic link anywhere below `dest` is refused rather than followed, on both the path built from the playbook and the one the host names back.

`fail_on_missing` (on by default, as in the reference) turns a missing source or a directory into an `ok` result carrying the reference's own message; any other failure, such as a read the agent refuses because the module server's per-stream limit was reached, still fails the task whatever `fail_on_missing` says. That limit is 4 MiB of `slurp`'s base64-encoded stdout, about 3 MiB of source file before encoding. `validate_checksum` checks the written file's SHA-1 against the host's before keeping it; a mismatch fails the task and removes what was written, unlike the reference, which leaves the mismatched file where it wrote it. A file being replaced keeps its existing mode, so a `0600` file fetched again stays `0600`.

## `service`

The host's init system names the module, the same way `package`'s manager does: `use:` in lower case unless `auto`, then `ansible_facts.service_mgr`, then a filtered `setup` on `ansible_service_mgr`. Unlike `package`, a name none of `systemd`, `systemd_service`, `sysvinit` carries falls back to `service` rather than failing the task, and so does a `setup` that failed. Naming `systemd` itself drops the arguments that module does not take: `pattern`, `runlevel`, `sleep`, `arguments`, `args`, each with its own warning, `Ignoring "<name>" as it is not used in "systemd"`; a task spelling it out as `ansible.builtin.systemd` keeps them.

A run naming both `package` and `service` carries every backend of both in the union before a single fact is known. Measured against ansible-core 2.19.12's own payload builder, that is about 170 KB more sent once per link than `apt` and `systemd_service` alone would cost.

## `unarchive`

An archive read on the controller, or already on the host, extracted by the `unarchive` module.

`copy:` folds into `remote_src` (the reference's own inversion), and the two together are rejected: `parameters are mutually exclusive: ('copy', 'remote_src')`. A `creates:` that already exists on the host skips the task, checked with a `stat` before `dest` is even looked at: `{"changed": false, "msg": "skipped, since <path> exists", "skipped": true}`; a `stat` that fails is read as "not there", the way a shell existence test would read a command it could not run, not as a task failure. The destination must exist and be a directory, or the task fails with `dest '<dest>' must be an existing dir`. Only then is the source searched, rejected if a host named it, staged and sent, unless `remote_src` names a file already there.

Rejected before the host is asked anything:

- `src (or content) and dest are required`
- `a '~' path is not supported yet on 'unarchive'`, for `dest` or `creates`
- A `src` that names nothing on the controller: `Task failed: Could not find or access '<src>' ...\nIf you are using a module and expect the file to exist on the remote, see the remote_src option`
- `src is a directory, not an archive: <src>`, for a `src` found as a directory on the controller.
- A source larger than one frame is rejected by its size, before it is read.

## A source file a managed host named

`copy`, `template` and `unarchive` all reject the same thing, in the same words: a `src` whose render read a managed host is never looked up.

```text
the 'src' of this task was named by a managed host, and a controller file a host chose is
never sent
```

This is stricter than the reference, on purpose. Measured on ansible-core 2.19.12, a `copy` (or `template`, or `unarchive`) whose `src` is a registered value, a command's `stdout` or a fact, sends whatever controller file that value names to the host, `~/.ssh` included. A host that controls what a command prints, or what a fact reports, can have any file the operator can read sent to it.

The check runs before the path is so much as looked up, not after a search that comes back empty: answering "not found" would already tell a host whether a path exists on the controller. `dest`, by contrast, is only ever a place to write to, never a file that gets read, so it is not rejected this way; `template_destpath` (see above) carries it through to the render as the host's own value when the host is the one who named it, same as any other untrusted string.

## What stays on the host

A file travels to a host only when a sub-task actually needs it, and never further than that task.

The controller stages it as a content-addressed blob, named by the BLAKE3 hash of its bytes, into a directory private to the one connection that sent it: `stage-<host>-<pid>/`, inside the agent's own cache, mode 0700 and rejected unless the agent owns it alone. The module reads the file from there, under a name of the agent's own choosing, never the controller's path. Once the sub-task that needed it has run, whether it succeeded, failed, or the module itself already consumed the file (as `copy` does when it moves its staged source into place), the agent removes what is left of it. Nothing waits for the connection to end: a rendered template often carries a secret, and a file nobody asked for a second time is not left for anyone to find later.

The connection's whole staging directory goes as well once the connection itself ends, whatever was left in it. A new agent starting on a host also removes the staging directories of dead agents of the same host it finds at start, so a run that was interrupted does not leave them behind forever. Two links to one host, such as two inventory names for one machine or two hosts delegating to the same one, never share a staging directory, since each is named by its own process id: one connection's staged file is never one another connection can see.
