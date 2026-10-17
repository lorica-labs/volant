---
title: Modules
description: Which modules Volant runs, where each one runs, and which ones are not supported yet.
---

<!-- Generated from crates/volant-protocol/src/modules.rs by `just docs-modules`. Do not edit by hand. -->

Volant runs a module in one of three places: natively in the agent on the host, on the controller itself, or on the host through the warm Python path. A playbook that names a module this release does not support yet stops when it loads, before the first task, the way Ansible stops on a module it cannot resolve.

A module named in a file that only a dynamic `include_tasks` or `include_role` brings in is checked when a host reaches that statement. The statement then fails for that host and nothing in the file runs. See [Includes and imports](/playbooks/includes/).

## On the agent

The agent runs these on the host, without Python. The arguments column lists what each module reads.

Ansible declares `chdir`, `creates` and `removes` as paths and expands `~` and `$VAR` in them before using the value, and `command` and `shell` do the same, from the task's `environment` and then the agent's own. The words of a `command` are expanded as Ansible expands them, from the task's `environment` and then the agent's own, so `command: /bin/echo $HOME` prints the home directory unless `expand_argument_vars` is false. A `shell` task has its shell do the expanding, and `expand_argument_vars` is refused there, as it is in Ansible.

| Module | Free-form | Arguments | What it does |
|---|---|---|---|
| `async_status` | no | `jid`, `mode` | Report on a job started with `async`, or remove its status file. |
| `command` | yes | `argv`, `chdir`, `cmd`, `creates`, `expand_argument_vars`, `removes`, `stdin`, `stdin_add_newline`, `strip_empty_ends` | Run a program directly, without a shell. |
| `raw` | yes | `executable` | Run a command line through the remote shell, with no module machinery around it. |
| `shell` | yes | `argv`, `chdir`, `cmd`, `creates`, `executable`, `removes`, `stdin`, `stdin_add_newline`, `strip_empty_ends` | Run a command line through a shell, `sh` unless `executable` names another. |

These arguments exist in Ansible and are not supported yet: a playbook that uses them stops before the run reaches a host.

- `shell`: `expand_argument_vars`

Over `ssh`, `raw` runs its command on a terminal, as ansible-core does with `ssh -tt`. The command's stderr is merged into stdout, every line ends in a carriage return and a newline, and stderr holds the line `ssh` prints when the session closes: `Shared connection to <host> closed.`, or `Connection to <host> closed.` when the connection is not shared. Set `ansible_ssh_use_tty` to false to run without a terminal. A local connection has none.

## On the controller

The controller runs these itself, so they need no connection to the host.

| Module | Free-form | What it does |
|---|---|---|
| `assert` | no | Fail the task unless every condition in `that` holds. |
| `debug` | no | Print a message or the value of a variable. |
| `fail` | no | Fail the task with a message. |
| `include_vars` | yes | Read a file of variables and set them on the host, for the rest of the run. |
| `pause` | no | Wait for `seconds` or `minutes`. On a terminal a pause with no duration asks for an answer and keeps it in `user_input`; `prompt` next to a duration is not supported yet. Without a terminal, a pause that asks for input prints a warning and continues at once, as Ansible does. |
| `set_fact` | no | Set facts for a host, for the rest of the run. |
| `validate_argument_spec` | no | Check a role's arguments against the specification in `meta/argument_specs.yml`. |

## On the warm Python path

Every other module ansible-core ships is a Python module, and Volant runs it as one. The host needs a Python 3 interpreter, and the controller needs ansible-core to build the payload. [The warm Python path](/internals/python/) explains how it works.

Some of these modules also have a native version in the agent. It answers when the task's arguments stay within what it implements. Otherwise, before it changes anything on the host, it hands the task to the Python module. The table below says which modules have one.

Modules from collections installed on the controller run the same way. The controller's ansible-core resolves each name before the first connection, and Volant never installs a collection. If a play or one of its roles names a module from a collection that is not installed, the run stops before it reaches a host and prints the `ansible-galaxy collection install` command that fixes it. Volant refuses, by name, a module that its collection runs through an action plugin, because it does not run a collection's action plugins. A module that only a dynamic include brings in is refused when a host reaches the include, which then fails for that host. Volant keeps a collection's module under its full name, so that module never stands in for a builtin module or for another collection's module with the same short name. [Modules from collections](/internals/python/#modules-from-collections) has the details.

The exceptions are the modules ansible-core runs through an action plugin that Volant does not have yet. What the playbook asks for lives in the plugin, not in the module, so sending the module alone would do something else and call it a success. Those modules are not supported yet: Volant names them before the run reaches a host. Fact gathering still works: a play's `gather_facts` runs the `setup` module directly.

| Module | How it runs |
|---|---|
| `apt` | natively in the agent when its arguments allow it, as a Python module otherwise |
| `apt_key` | Python module |
| `apt_repository` | Python module |
| `blockinfile` | Python module |
| `cron` | Python module |
| `deb822_repository` | Python module |
| `debconf` | Python module |
| `dnf5` | Python module |
| `dpkg_selections` | Python module |
| `expect` | Python module |
| `file` | natively in the agent when its arguments allow it, as a Python module otherwise |
| `find` | Python module |
| `get_url` | Python module |
| `getent` | Python module |
| `git` | Python module |
| `group` | natively in the agent when its arguments allow it, as a Python module otherwise |
| `hostname` | Python module |
| `iptables` | Python module |
| `known_hosts` | Python module |
| `lineinfile` | natively in the agent when its arguments allow it, as a Python module otherwise |
| `mount_facts` | Python module |
| `package_facts` | natively in the agent when its arguments allow it, as a Python module otherwise |
| `ping` | Python module |
| `pip` | Python module |
| `replace` | Python module |
| `rpm_key` | Python module |
| `service_facts` | natively in the agent when its arguments allow it, as a Python module otherwise |
| `setup` | natively in the agent when its arguments allow it, as a Python module otherwise |
| `slurp` | Python module |
| `stat` | natively in the agent when its arguments allow it, as a Python module otherwise |
| `subversion` | Python module |
| `systemd` | natively in the agent when its arguments allow it, as a Python module otherwise |
| `systemd_service` | natively in the agent when its arguments allow it, as a Python module otherwise |
| `sysvinit` | Python module |
| `tempfile` | Python module |
| `user` | natively in the agent when its arguments allow it, as a Python module otherwise |
| `wait_for` | Python module |
| `yum_repository` | Python module |

## Through an action plugin

The controller runs these as the reference's action plugins do: it picks or renders what reaches the host, then sends it as ordinary modules over the connection the task already has. `reboot` also waits for the host: it reconnects until the host answers with a new boot id. [Action plugins](/reference/action-plugins/) describes each one.

| Module | What it does |
|---|---|
| `add_host` | Add a host, or groups to a host, in the running play's inventory. |
| `assemble` | Join the fragments of a directory into one file on the host. |
| `copy` | Copy a file from the controller, or `content:`, to the host. |
| `dnf` | Install or remove packages with `dnf` or `dnf5`, whichever the host runs. |
| `fetch` | Copy a file from the host to the controller, under `dest` and nowhere else. |
| `gather_facts` | Gather the host's facts as a task. |
| `group_by` | Put the host in a group named from its variables. |
| `package` | Install or remove packages with the host's own package manager. |
| `reboot` | Reboot the host and wait until it has booted again. |
| `script` | Copy a script from the controller to the host and run it there. |
| `service` | Manage a service with the host's own init system. |
| `set_stats` | Record custom statistics for the recap. |
| `template` | Render a template on the controller and copy the result to the host. |
| `unarchive` | Extract an archive read on the controller, or one already on the host. |
| `uri` | Send an HTTP request from the host. |
| `wait_for_connection` | Wait until the host answers again. |
