# Modules

These are the modules Volant runs. A playbook naming any other module is refused when it is loaded, before the first task, the way Ansible refuses a module it cannot resolve. A file a dynamic `include_tasks` or `include_role` names is read when a host reaches the statement, so a module named there is refused at that moment instead: the statement fails for the host that asked, and nothing in the file runs. What `import_tasks` and `import_role` name is compiled with the play and checked with it. Everything else waits on the warm Python path.

## On the agent

The agent runs these on the host, without Python. The arguments column lists what each module reads, and each is read as the playbook writes it. Ansible declares `chdir`, `creates` and `removes` as paths, which expands `~` and `$VAR` in them before the value is used; this release does not, so `creates: ~/.provisioned` looks for a directory named `~`. No other argument is expanded either. Ansible runs `command: /bin/echo $HOME` with the variable already replaced, and here the program is handed the five characters `$HOME`. A `shell` task prints the same thing under both engines, because there the shell does the expanding rather than the module. The list under the table names the arguments Ansible has and this release refuses.

| Module | Free-form arguments | Arguments | What it does |
|---|---|---|---|
| `command` | yes | `argv`, `chdir`, `cmd`, `creates`, `removes`, `stdin`, `stdin_add_newline`, `strip_empty_ends` | Run a program directly, without a shell. |
| `raw` | yes | `executable` | Run a command line through the remote shell, with no module machinery around it. |
| `shell` | yes | `argv`, `chdir`, `cmd`, `creates`, `executable`, `removes`, `stdin`, `stdin_add_newline`, `strip_empty_ends` | Run a command line through a shell, `sh` unless `executable` names another. |

Volant refuses these before the run reaches a host:

- `command`: `expand_argument_vars: true`
- `shell`: `expand_argument_vars`

## On the controller

The controller runs these itself, so they need no connection to the host.

| Module | Free-form arguments | What it does |
|---|---|---|
| `assert` | no | Fail the task unless every condition in `that` holds. |
| `debug` | no | Print a message or the value of a variable. |
| `fail` | no | Fail the task with a message. |
| `include_vars` | yes | Read a file of variables and set them on the host, for the rest of the run. |
| `pause` | no | Wait for `seconds` or `minutes`. Volant cannot read an answer from the keyboard, so when standard input is a terminal it refuses a `prompt`, and a pause with no duration, where Ansible would wait for one. Without a terminal, a pause that asks for an answer prints a warning and goes on at once, as Ansible does. |
| `set_fact` | no | Set facts for a host, for the rest of the run. |
| `validate_argument_spec` | no | Check a role's arguments against the specification in `meta/argument_specs.yml`. |

## On the warm Python path

Everything else ansible-core ships is a Python module, and Volant runs it as one. The modules a run needs travel to the host together, once, in a single archive named by its own content. A Python server the agent keeps warm runs each of them in a fork of itself. The agent keeps the archive, so a host that already has it is sent nothing, and the interpreter comes from the list the agent reported when it started.

The exceptions are the modules the reference runs through an action plugin Volant does not have yet. Volant refuses those by name before the run reaches a host, because what the playbook asks for lives in the plugin and not in the module. Sending the module on its own would run something else and call it a success.

| Module | How it runs |
|---|---|
| `add_host` | refused: waits on the `add_host` action plugin |
| `apt` | Python module |
| `apt_key` | Python module |
| `apt_repository` | Python module |
| `assemble` | refused: waits on the `assemble` action plugin |
| `async_status` | refused: waits on the `async_status` action plugin |
| `blockinfile` | Python module |
| `cron` | Python module |
| `deb822_repository` | Python module |
| `debconf` | Python module |
| `dnf` | refused: waits on the `dnf` action plugin |
| `dnf5` | Python module |
| `dpkg_selections` | Python module |
| `expect` | Python module |
| `fetch` | refused: waits on the `fetch` action plugin |
| `file` | Python module |
| `find` | Python module |
| `gather_facts` | refused: waits on the `gather_facts` action plugin |
| `get_url` | Python module |
| `getent` | Python module |
| `git` | Python module |
| `group` | Python module |
| `group_by` | refused: waits on the `group_by` action plugin |
| `hostname` | Python module |
| `iptables` | Python module |
| `known_hosts` | Python module |
| `lineinfile` | Python module |
| `mount_facts` | Python module |
| `package_facts` | Python module |
| `ping` | Python module |
| `pip` | Python module |
| `reboot` | refused: waits on the `reboot` action plugin |
| `replace` | Python module |
| `rpm_key` | Python module |
| `script` | refused: waits on the `script` action plugin |
| `service_facts` | Python module |
| `set_stats` | refused: waits on the `set_stats` action plugin |
| `setup` | Python module |
| `slurp` | Python module |
| `stat` | Python module |
| `subversion` | Python module |
| `systemd` | Python module |
| `systemd_service` | Python module |
| `sysvinit` | Python module |
| `tempfile` | Python module |
| `uri` | refused: waits on the `uri` action plugin |
| `user` | Python module |
| `wait_for` | Python module |
| `wait_for_connection` | refused: waits on the `wait_for_connection` action plugin |
| `yum_repository` | Python module |

## Through an action plugin

The controller runs these as the reference's action plugins do: it picks or renders what reaches the host, then sends it as ordinary Python modules over the connection the task already has. [Action plugins](actions.md) describes each one.

| Module | What it does |
|---|---|
| `copy` | Copy a file from the controller, or `content:`, to the host. |
| `package` | Install or remove packages with the host's own package manager. |
| `service` | Manage a service with the host's own init system. |
| `template` | Render a template on the controller and copy the result to the host. |
| `unarchive` | Extract an archive read on the controller, or one already on the host. |
