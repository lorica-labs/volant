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

Which modules those are follows a rule rather than a list: every builtin in neither table above, except the ones the reference runs through an action plugin. Volant refuses those by name before the run reaches a host, because what the playbook asks for lives in the plugin and not in the module. `package` picks the host's package manager, and `template` renders the file on the controller before the task is sent. Sending the module on its own would run something else and call it a success.
