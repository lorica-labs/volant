---
title: Action plugins
description: How Volant runs the modules ansible-core runs through an action plugin on the controller, in Rust or on the Python bridge.
---

Most modules ansible-core ships run as themselves: the controller sends a payload, the host runs it, the result comes back. A handful run differently: the reference itself decides what to send, sometimes after asking the host something first, and the module name on the task is only the entry point. `package` is one: what runs is `apt` or `dnf`, chosen after the host says which one it has. `copy` is another: whether anything travels to the host at all depends on a checksum the host reports back first.

Volant runs eight of these itself: `copy`, `dnf`, `fetch`, `package`, `reboot`, `service`, `template` and `unarchive`. Each one is a small state machine on the controller: asked for the next step, handed the result of the last one, until it has the task's own result. Every step it takes is an ordinary module of the run's union, sent alone over the link the task already has. The result a plugin ends with goes down the same road every other module result takes, so nothing a host answered reaches a playbook's variables by a way of its own.

A task's own `timeout:` keyword bounds the whole item this way, on any of the eight: every sub-task and every wait for the host to come back, `reboot`'s reconnecting included. Running out of it ends the task there with `Task failed: Timed out after <seconds> second(s).`, never whatever the plugin would have made of the time it had left.

The names the reference also runs through an action plugin, and every action plugin of the collections in the corpus, run on a second road: the plugin's own Python, from ansible-core, in the controller's helper. See [On the bridge](#on-the-bridge) below. See [Modules](/reference/modules/) for what this release runs natively, on the controller, and on the warm Python path.

## `copy`

A controller file, or a `content:` written straight into the destination, sent to the host only when it differs.

The plugin runs `stat` on the destination first, asking for a SHA-1 unless `force: false`, in which case it asks for none and leaves an existing destination untouched whatever it holds. It compares that sum with the one of the local bytes. A destination already holding the same bytes gets `file`, to set the attributes the task asked for; everything else gets the bytes staged on the host and a `copy` sub-task. Only that last case sends anything.

`remote_src: true` skips all of this: `copy` runs alone, with the task's own arguments, and `src` names a file already on the host.

Rejected before the host is asked anything, in the reference's own words:

- `src (or content) is required`
- `dest is required`
- `src and content are mutually exclusive`
- `can not use content with a dir as dest`
- `copying a directory is not supported yet: <src>`. The Rust plugin does not copy a directory, so the task runs on the bridge, which does.
- A `src` that names nothing on the controller, wrapped as `Unexpected AnsibleActionFail error: Could not find or access '<src>' ...`.
- A source larger than one frame is rejected by its size, before it is read.

## `template`

A template rendered on the controller, then handed to the same path `copy` uses.

The plugin finds the template under `templates/`, renders it once with the task's variables and its own delimiters, and copies the result the way `copy` copies a `content:`: `stat`, then `file` or a staged `copy`. Rendering happens exactly once; nothing renders the result a second time, so a value a managed host contributed lands in the file as text rather than as a template of its own.

Four variables are set for the render, on top of the task's own: `ansible_managed` (unless the task already set it), `template_path` (`src` as the task wrote it), `template_fullpath` (the path found on the controller) and `template_destpath` (`dest` as rendered, see below). `template_host`, `template_uid`, `template_run_date` and `template_mtime`, which the reference also sets, are not.

`trim_blocks` defaults on and `lstrip_blocks` defaults off, as in the reference, and the file's trailing newline is kept. `newline_sequence` takes `\n`, `\r` or `\r\n`, the escaped four-character spelling read the same as the literal one. The six delimiter options (`variable_start_string` and its five relatives) and a `#jinja2:` first line in the template file are read as the reference reads them, see [Rendering a template file](/variables/templating/#rendering-a-template-file). `output_encoding` writes UTF-8, UTF-8 with a signature, Latin-1, ASCII, cp1252, ISO 8859-15, UTF-16 and UTF-32, and names any other encoding as not supported yet. A template that uses `{% include %}` or `{% import %}` fails: nothing here loads a second file mid-render.

Rejected before the host is asked anything:

- `'state' cannot be specified on a template`
- `src and dest are required`
- `newline_sequence needs to be one of: \n, \r or \r\n`
- `Block, variable and comment start strings must be different.`
- `TemplateOverrides.<name> must be <class 'str'> instead of <class 'NoneType'>`, for a delimiter option that is empty or null
- `output_encoding '<encoding>' is not supported yet on 'template': only utf-8, ... are`
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

The module `yum` is not a module of its own in ansible-core 2.19: `yum`, `ansible.builtin.yum` and `ansible.legacy.yum` are redirects to the `dnf` action, and a task that names one runs this plugin. A `module_defaults` key for `yum` applies to a `dnf` task, and the reverse.

## `fetch`

A file read from a host, written onto the controller under `dest` and nowhere else.

Without `become` the plugin runs `stat` on the source first and leaves an already-matching local file alone; under `become` it goes straight to `slurp`. The bytes always travel back through `slurp`, decoded strictly, never rendered and never put in a variable. The destination is `dest/<inventory_hostname>/<src>`, unless `flat: true` names `dest` itself directly, or, with a trailing slash, `dest/<basename of src>`.

Measured on ansible-core 2.19.12, a relative `src` climbing with `..` makes the reference write outside `dest` on the controller: a `src` of `../../../../../../../../tmp/x` without `flat` writes `/tmp/x`. Here every candidate path is built and normalised without touching the filesystem, and refused unless it sits under `dest`, before a directory is even created:

- `the fetched path <p> is outside '<dest>'`

This is checked twice: once on the `src` the playbook wrote, and again on the path the host names back, which does not have to match. A `dest` whose render read a managed host is refused before anything is asked unless `[volant] host_named_dest_dirs` covers it (see [A dest a managed host named](#a-dest-a-managed-host-named)), and so is a loop's `inventory_hostname` when a host set it, or one that is not a single path component:

- `the 'dest' of this task was named by a managed host, and a controller path a host chose is never written unless host_named_dest_dirs names a directory above it`
- `the inventory_hostname '<name>' is not one path component, and fetch files what it fetches under it`

A symbolic link anywhere below `dest` is refused rather than followed, on both the path built from the playbook and the one the host names back.

`fail_on_missing`, on by default as in the reference, fails the task on a missing source or a directory; `fail_on_missing: false` turns either into an `ok` result carrying the reference's own message instead. Either way, any other failure, such as a read the agent refuses because the module server's per-stream limit was reached, still fails the task. That limit is 4 MiB of `slurp`'s base64-encoded stdout, about 3 MiB of source file before encoding. `validate_checksum` checks the written file's SHA-1 against the host's before keeping it; a mismatch fails the task and removes what was written, unlike the reference, which leaves the mismatched file where it wrote it. A file being replaced keeps its existing mode, so a `0600` file fetched again stays `0600`.

### A dest a managed host named

A `dest` is named by a managed host when its render read something a host contributed. A real case is `dest: "{{ tmp.path }}/out/"`, where `tmp` is the registered result of a `tempfile` task. ansible-core writes there. A host that answers `tempfile` with `/home/<operator>/.ssh` would make the controller write as the operator under that directory, so by default `fetch` refuses it and names the setting.

`[volant] host_named_dest_dirs` (environment: `VOLANT_HOST_NAMED_DEST_DIRS`) is a colon-separated list of absolute directories, empty by default. A `dest` a host named is written only when the path it resolves to on the file system lies strictly inside one of them:

- The `dest` must be absolute and hold no `..`. A relative path and a path that starts with `~` are refused, never expanded.
- The deepest part of the path that exists is resolved through every link, and the rest is appended. A link that leaves the directory, a link that points nowhere, the directory itself and a sibling that shares its name as a prefix (`/srv/fetch-evil` for `/srv/fetch`) are refused. The check runs again just before the write.
- The write walks down from the directory of the list and refuses any link below it, even one that stays inside, and any existing directory below it that another user owns or that every user can write without the sticky bit. Another user could otherwise swap either for a link between the check and the write. The directories `fetch` creates on the way are its own.
- A directory of the list that the group or other users can write to holds nothing, unless its sticky bit is set, as it is on `/tmp`. This is checked when a task uses the directory.
- An entry that is relative, empty, written with `~` or `..`, or `/`, stops the run at start with exit code 5.
- A `dest` the playbook wrote is not held to the list, and the setting changes nothing for `src`, which names a file on the host.

```
the 'dest' of this task was named by a managed host, and '/home/op/.ssh/x/' is not inside any of the directories (host_named_dest_dirs = /srv/fetch)
```

Name a directory that only the operator can write. With `/tmp` in the list a host can pick any name below it.

Only `fetch` is held to the list. `copy` and `template` with `delegate_to: localhost`, and `synchronize` in pull mode, write a `dest` on the controller too. They are not refused when a host named it, as in ansible-core and as before.

## `service`

The host's init system names the module, the same way `package`'s manager does: `use:` in lower case unless `auto`, then `ansible_facts.service_mgr`, then a filtered `setup` on `ansible_service_mgr`. Unlike `package`, a name none of `systemd`, `systemd_service`, `sysvinit` carries falls back to `service` rather than failing the task, and so does a `setup` that failed. Naming `systemd` itself drops the arguments that module does not take: `pattern`, `runlevel`, `sleep`, `arguments`, `args`, each with its own warning, `Ignoring "<name>" as it is not used in "systemd"`; a task spelling it out as `ansible.builtin.systemd` keeps them.

A run naming both `package` and `service` carries every backend of both in the union before a single fact is known. Measured against ansible-core 2.19.12's own payload builder, that is about 170 KB more sent once per link than `apt` and `systemd_service` alone would cost.

## `unarchive`

An archive read on the controller, or already on the host, extracted by the `unarchive` module.

`copy:` folds into `remote_src` (the reference's own inversion), and the two together are rejected: `parameters are mutually exclusive: ('copy', 'remote_src')`. A `creates:` that already exists on the host skips the task, checked with a `stat` before `dest` is even looked at: `{"changed": false, "msg": "skipped, since <path> exists", "skipped": true}`; a `stat` that fails is read as "not there", the way a shell existence test would read a command it could not run, not as a task failure. The destination must exist and be a directory, or the task fails with `dest '<dest>' must be an existing dir`. Only then is the source searched, rejected if a host named it, staged and sent, unless `remote_src` names a file already there.

Rejected before the host is asked anything:

- `src (or content) and dest are required`
- `a '~' path is not supported yet on 'unarchive'`, for `dest` or `creates`. The task then runs on the bridge.
- A `src` that names nothing on the controller: `Task failed: Could not find or access '<src>' ...\nIf you are using a module and expect the file to exist on the remote, see the remote_src option`
- `src is a directory, not an archive: <src>`, for a `src` found as a directory on the controller.
- A source larger than one frame is rejected by its size, before it is read.

## `reboot`

Restart the host, then wait for it to answer with a new boot id and its test command to succeed, before the task ends.

Read off `plugins/action/reboot.py` of ansible-core 2.19.12 and measured against it, under `become`, on a host that came back on its own: a `setup` filtered to the distribution, `cat /proc/sys/kernel/random/boot_id` run as a bare command, a `find` across five search paths for the shutdown program, the shutdown command itself, then the boot id read again until it changes, and finally the test command. All of these run over the host's existing connection, through `raw`, the same way the reference's low-level command runs over its own.

Run against `local`, the connection the controller itself uses, the task is rejected before anything else: `Running reboot with local connection would reboot the control node.`

Once the shutdown command has gone out, this release drops the connection and opens a fresh one to check on the host, rather than holding the old one open across the reboot. `post_reboot_delay` is how long it waits before the first check, and `reboot_timeout` (600 seconds by default) bounds the whole wait unless the task's own `timeout:` cuts it short first. `connect_timeout`, when the task gives one, only sets each reconnection's own SSH `ConnectTimeout`; it no longer bounds the try itself. An empty boot id answer is not read as a new boot: a host can answer nothing in the moment just before it goes down. Once the boot id has changed, `test_command` (`whoami` by default) still has to succeed before the task is done; either wait running out fails the task with the reference's own message, `Timed out waiting for <what> (timeout=<reboot_timeout>)`. A link another host of the same run kept open to the one just rebooted gets one extra liveness check before its next use, and is replaced if the reboot killed it: this driver only drops its own links, not a delegate's.

The shutdown line follows the reference's own table: `shutdown -r <minutes> "<msg>"` on most distributions, bare `reboot` on Alpine, `shutdown -r +<minutes> "<msg>"` on Void, or `reboot_command` split at its first space when the task gives one. `search_paths` (five directories by default) is where the shutdown program is found when `reboot_command` does not already name an absolute path; not finding it there fails the task, naming every path searched.

Rejected before the host is asked anything: an argument outside the reference's own nine (`boot_time_command`, `connect_timeout`, `msg`, `post_reboot_delay`, `pre_reboot_delay`, `reboot_command`, `reboot_timeout`, `search_paths`, `test_command`) fails the task, sorted and named: `Invalid options for reboot: <name>[,<name>...]`.

## On the bridge

`add_host`, `assemble`, `async_status`, `gather_facts`, `group_by`, `script`, `set_stats`, `uri` and `wait_for_connection` run ansible-core's own action plugin in the helper, and so does every action plugin a collection ships (the plugins that need a Windows host or another connection type excepted; the [parity](/reference/parity/) page counts them). `copy` and `unarchive` take the same road for the two shapes the Rust plugins refuse: `copy` of a directory, and `unarchive` with a `~` in `dest` or `creates`. Under `host_named_paths = refuse`, a `src` a managed host named never takes it: the Rust plugin's refusal stands.

The plugin asks for what it needs and the controller answers: run this module, run this command, read a variable, make a temporary directory, put this file. Every answer is bounded by the task's `timeout:`. A plugin that never returns, or a helper that dies, fails that task and not the run; a temporary directory the plugin left on the host is removed. `add_host`, `group_by` and `set_stats` need no host and run without a connection. What they return is applied by the executor: new hosts and groups for the next tasks, and the custom stats printed after the recap. A value of `add_host` rendered from a fact stays data: it is kept as a fact of the new host, never as an inventory variable. So does a host name or a group name of `add_host` or `group_by` that was built from text a managed host wrote. So does any host name or group name that holds `{`, whether the inventory files, a dynamic inventory source, `meta: refresh_inventory`, `add_host` or `group_by` gave it: `groups`, `hostvars`, `group_names`, the host lists of a play and `inventory_hostname` show it as text, as ansible-core does. Once one such name exists, every read of the lists that carry it is data. Reading one host through `hostvars[name]` is data only for that host, so a nested template of another host still renders.

The helper stands in for the controller's connection, so some things the reference lets a plugin do are refused, by name, before they do anything:

- A member of the connection or play context that is not one of the few the shim carries: `Connection.<name> is not supported yet`, `the connection option <name> is not supported yet`, `the play context field <name> is not supported yet`, `the shell plugin <name> is not supported yet`.
- An `async_status` mode other than `status` and `cleanup`.
- A module that is not UTF-8 text where the shim reads it as text.
- The task keywords `action`, `local_action` and `module_defaults` inside the helper: the controller merged the defaults already.

Three behaviours differ from the reference on purpose:

- `wrap_async` is emulated. The module runs to its end when the plugin asks for it, and `async_status` then answers that the job finished. Parallel jobs run one after the other. A job's `async:` limit is judged after the module ran, as `Timeout exceeded`; the module is not interrupted.
- A command a plugin sends with `sudoable=False` runs on the task's own link, so under `become` it runs escalated.

### A plugin that makes its own connection

`ansible.posix.synchronize` over ssh does not run its module on the host. It makes a `local` connection of its own and runs the module on the controller, where `rsync -e ssh` copies the files to or from the host. The helper gives such a plugin ansible-core's own methods for running a module, and keeps its stand-in connection for the connection the task came with.

The plugin reads the link's key and user (the host variable, `--private-key`, `-u` or `ansible.cfg`), whether the task escalates and as whom (`sudo` becomes `--rsync-path='sudo -u <user> rsync'`), `ansible_port`, `delegate_to` and `ansible_delegated_vars`, and, with `use_ssh_args`, the host's `ansible_ssh_common_args` and `ansible_ssh_extra_args`. `ssh_args` is ansible-core's default, `-C -o ControlMaster=auto -o ControlPersist=60s`: an `ssh_args` set in `ansible.cfg` is not read. `rsync` has to be on the controller and on the host. Push, pull, `delete`, `rsync_opts`, `delegate_to`, `become` with `sudo`, `--check` and `--diff` give the output `ansible-playbook` gives.

### Known gaps

- `wait_for_connection` on a host that is down from the first connection ends `UNREACHABLE` for a plugin item that never got a link; the reference reports `failed` with `timed out waiting for ping module test`.
- A bridged result that is `unreachable` at the result level prints as `ok`.
- A skipped bridged result carries `"failed": false`.
- The warnings a plugin writes with `display` go to the helper's standard error, not to the task's output; so does ansible-core's `Error loading plugin` warning for an unknown collection name.
- A module that `runtime.yml` routes to a differently named action plugin runs under the task's own name.
- `fact_diff` prints no diff, because `diff` is not supported yet.
- `ansible_play_hosts_all` does not grow for a host added inside a `serial` play, and a delegated `add_host` or `group_by` applies to the task's own host.

## A source file a managed host named

A `src` is named by a managed host when its render read something a host contributed: a gathered fact, as in `src: "{{ ansible_os_family }}.j2"`, a registered result, or an item of `with_first_found` built from one. `[volant] host_named_paths` says what `copy`, `template`, `script`, `unarchive` and `assemble` do with it. It applies to the file read on the controller, and under `--check` and `--diff` too. For `template`, a delimiter option (`variable_start_string` and the five others) whose value a host contributed is used under `allow`, as ansible-core uses it, and refused under `contained` and `refuse`. A `#jinja2:` first line in an author's template is read in every mode.

| Value | What the task does |
|---|---|
| `allow` (the default) | Looks the file up and sends it, as ansible-core does. |
| `contained` | Reads the file only when it is in a `files` or `templates` directory that the task searches. |
| `refuse` | Fails the task before any path is looked up. |

`allow` means that a managed host can choose which readable controller file a `copy`, `template` or `script` task sends to it, exactly as in ansible-core 2.19.12. Measured there, a `src` that is a registered value, a command's `stdout` or a fact sends whatever file it names, `~/.ssh` included. Roles that pick a template by fact depend on this. `contained` and `refuse` exist to tighten it, for a run where a host is less trusted than the person who runs the playbook.

Under `contained`, the directories that count are the `files` and `templates` directories of the role, of the roles it is a dependency of, of the directory of the task's file (`tasks/files` in a role) and of the playbook directory. The rest of those trees does not count: `group_vars`, `vars`, `defaults`, an inventory, `ansible.cfg` and a vault password file are never reachable by a name a host chose, and neither is a file kept beside the playbook. The rules, in order:

- A name that starts with `~`, or `~user`, is refused. ansible-core's own plugins expand it to a home directory, which the text of the name does not show.
- A name with a `..` component is refused, even one that folds back inside. The file system follows a link before it folds `..`, so the folded path is not the one that would be opened.
- An absolute name must lie under one of those directories and must not hold a `$`, which ansible-core expands in an absolute name. `with_first_found` hands over an absolute name of this kind.
- A relative name is looked up as the plugin looks it up. The first file that exists is the one read, and it must lie under one of those directories. A file with that name beside the playbook, outside `files`, is refused when it exists. As a result the refusal tells a host that such a file exists. This is limited to a file at `<playbook>/<name>`, `<role>/<name>`, `<role>/tasks/<name>` or `<parent role>/<name>`; the answer for any other name is the usual "could not find".

The check reads the text of the path. A link that you placed inside those directories is your own file and is followed. A source on the host, such as `remote_src: true` for `copy` and `unarchive`, or `assemble` when `remote_src` is not set to false, is not a controller file and is not checked. `template`, `script` and the plugins of a collection read no `remote_src`, so it never exempts them. For `script`, the file is the first word of the line, or of `cmd`. For `uri`, `src` and the `filename` of each field of a multipart `body` count.

A task is refused before the path is looked up when the mode or the text of the name decides it, because an answer of "not found" would already tell a host whether a path exists on the controller. Under `refuse` the message is:

```text
the 'src' of this task was named by a managed host, and a controller file a host chose is
never sent
```

Under `contained`, it names the value and the setting:

```text
the 'src' of this task was named by a managed host, and '../up.j2' is not inside the
files or templates directories of the playbook or the role (host_named_paths = contained)
```

The argument is `_raw_params` or `cmd` for a `script`. Any other word for the setting stops the run at start with exit code 5 and names the key, the three words and the value found.

The setting covers the tasks listed above. It does not cover `include_vars`, `vars_files` or a lookup such as `file` whose path a host named. Nor does it cover a `copy` with `remote_src` or a `slurp` delegated to `localhost` that reads a controller path a host named. These behave as in ansible-core, and the same way on every earlier version of Volant.

The mode changes which file is read, never how the text of a template is trusted. When the `src` of a `template` was named by a host, `template_path` and `template_fullpath` are that host's values in the render, the way `template_destpath` is for a `dest`.

`fetch` is outside this setting. Its `dest` is a place the controller writes, not a file it reads, and `host_named_dest_dirs` decides what happens to one a host named (see [A dest a managed host named](#a-dest-a-managed-host-named)). `dest` of the other plugins is only ever a place to write on the host, so it is not refused; `template_destpath` carries it into the render as the host's own value.

## What stays on the host

A file travels to a host only when a sub-task actually needs it, and never further than that task.

The controller stages it as a content-addressed blob, named by the BLAKE3 hash of its bytes, into a directory private to the one connection that sent it: `stage-<host>-<pid>/`, inside the agent's own cache, mode 0700 and rejected unless the agent owns it alone. The module reads the file from there, under a name of the agent's own choosing, never the controller's path. Once the sub-task that needed it has run, whether it succeeded, failed, or the module itself already consumed the file (as `copy` does when it moves its staged source into place), the agent removes what is left of it. Nothing waits for the connection to end: a rendered template often carries a secret, and a file nobody asked for a second time is not left for anyone to find later.

The connection's whole staging directory goes as well once the connection itself ends, whatever was left in it. A new agent starting on a host also removes the staging directories of dead agents of the same host it finds at start, so a run that was interrupted does not leave them behind forever. Two links to one host, such as two inventory names for one machine or two hosts delegating to the same one, never share a staging directory, since each is named by its own process id: one connection's staged file is never one another connection can see.
