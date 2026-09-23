---
title: Modules
description: Which modules Volant runs, where each one runs, and which ones are not supported yet.
---

<!-- Generated from crates/volant-protocol/src/modules.rs by `just docs-modules`. Do not edit by hand. -->

Volant runs a module in one of three places: natively in the agent on the host, on the controller itself, or on the host through the warm Python path. A playbook that names a module this release does not support yet stops when it loads, before the first task, the way Ansible stops on a module it cannot resolve.

A module named in a file that only a dynamic `include_tasks` or `include_role` brings in is checked when a host reaches that statement. The statement then fails for that host and nothing in the file runs. See [Includes and imports](/playbooks/includes/).

## On the agent

The agent runs these on the host, without Python. The arguments column lists what each module reads.

Each argument is read as the playbook writes it. Ansible declares `chdir`, `creates` and `removes` as paths and expands `~` and `$VAR` in them before using the value. This release does not, so `creates: ~/.provisioned` looks for a directory literally named `~`. No other argument is expanded either: Ansible runs `command: /bin/echo $HOME` with the variable already replaced, while Volant hands the program the five characters `$HOME`. A `shell` task prints the same thing under both engines, because there the shell does the expanding, not the module.

| Module | Free-form | Arguments | What it does |
|---|---|---|---|
| `command` | yes | `argv`, `chdir`, `cmd`, `creates`, `removes`, `stdin`, `stdin_add_newline`, `strip_empty_ends` | Run a program directly, without a shell. |
| `raw` | yes | `executable` | Run a command line through the remote shell, with no module machinery around it. |
| `shell` | yes | `argv`, `chdir`, `cmd`, `creates`, `executable`, `removes`, `stdin`, `stdin_add_newline`, `strip_empty_ends` | Run a command line through a shell, `sh` unless `executable` names another. |

These arguments exist in Ansible and are not supported yet: a playbook that uses them stops before the run reaches a host.

- `command`: `expand_argument_vars: true`
- `shell`: `expand_argument_vars`

## On the controller

The controller runs these itself, so they need no connection to the host.

| Module | Free-form | What it does |
|---|---|---|
| `assert` | no | Fail the task unless every condition in `that` holds. |
| `debug` | no | Print a message or the value of a variable. |
| `fail` | no | Fail the task with a message. |
| `include_vars` | yes | Read a file of variables and set them on the host, for the rest of the run. |
| `pause` | no | Wait for `seconds` or `minutes`. Volant cannot read keyboard input, so when standard input is a terminal, `prompt` and a pause with no duration are not supported yet. Without a terminal, a pause that asks for input prints a warning and continues at once, as Ansible does. |
| `set_fact` | no | Set facts for a host, for the rest of the run. |
| `validate_argument_spec` | no | Check a role's arguments against the specification in `meta/argument_specs.yml`. |

## On the warm Python path

Every other module ansible-core ships is a Python module, and Volant runs it as one. The host needs a Python 3 interpreter, and the controller needs ansible-core to build the payload. [The warm Python path](/internals/python/) explains how it works.

Modules from collections installed on the controller run the same way. The controller's ansible-core resolves each name before the first connection, and Volant never installs a collection. If a play or one of its roles names a module from a collection that is not installed, the run stops before it reaches a host and prints the `ansible-galaxy collection install` command that fixes it. Volant refuses, by name, a module that its collection runs through an action plugin, because it does not run a collection's action plugins. A module that only a dynamic include brings in is refused when a host reaches the include, which then fails for that host. Volant keeps a collection's module under its full name, so that module never stands in for a builtin module or for another collection's module with the same short name.

The exceptions are the modules ansible-core runs through an action plugin that Volant does not have yet. What the playbook asks for lives in the plugin, not in the module, so sending the module alone would do something else and call it a success. Those modules are not supported yet: Volant names them before the run reaches a host. Fact gathering still works: a play's `gather_facts` runs the `setup` module directly.

| Module | How it runs |
|---|---|
| `add_host` | not supported yet: needs the `add_host` action plugin |
| `apt` | Python module |
| `apt_key` | Python module |
| `apt_repository` | Python module |
| `assemble` | not supported yet: needs the `assemble` action plugin |
| `async_status` | not supported yet: needs the `async_status` action plugin |
| `blockinfile` | Python module |
| `cron` | Python module |
| `deb822_repository` | Python module |
| `debconf` | Python module |
| `dnf5` | Python module |
| `dpkg_selections` | Python module |
| `expect` | Python module |
| `file` | Python module |
| `find` | Python module |
| `gather_facts` | not supported yet: needs the `gather_facts` action plugin |
| `get_url` | Python module |
| `getent` | Python module |
| `git` | Python module |
| `group` | Python module |
| `group_by` | not supported yet: needs the `group_by` action plugin |
| `hostname` | Python module |
| `iptables` | Python module |
| `known_hosts` | Python module |
| `lineinfile` | Python module |
| `mount_facts` | Python module |
| `package_facts` | Python module |
| `ping` | Python module |
| `pip` | Python module |
| `reboot` | not supported yet: needs the `reboot` action plugin |
| `replace` | Python module |
| `rpm_key` | Python module |
| `script` | not supported yet: needs the `script` action plugin |
| `service_facts` | Python module |
| `set_stats` | not supported yet: needs the `set_stats` action plugin |
| `setup` | Python module |
| `slurp` | Python module |
| `stat` | Python module |
| `subversion` | Python module |
| `systemd` | Python module |
| `systemd_service` | Python module |
| `sysvinit` | Python module |
| `tempfile` | Python module |
| `uri` | not supported yet: needs the `uri` action plugin |
| `user` | Python module |
| `wait_for` | Python module |
| `wait_for_connection` | not supported yet: needs the `wait_for_connection` action plugin |
| `yum_repository` | Python module |

## Through an action plugin

The controller runs these as the reference's action plugins do: it picks or renders what reaches the host, then sends it as ordinary Python modules over the connection the task already has. [Action plugins](/reference/action-plugins/) describes each one.

| Module | What it does |
|---|---|
| `copy` | Copy a file from the controller, or `content:`, to the host. |
| `dnf` | Install or remove packages with `dnf` or `dnf5`, whichever the host runs. |
| `fetch` | Copy a file from the host to the controller, under `dest` and nowhere else. |
| `package` | Install or remove packages with the host's own package manager. |
| `service` | Manage a service with the host's own init system. |
| `template` | Render a template on the controller and copy the result to the host. |
| `unarchive` | Extract an archive read on the controller, or one already on the host. |
