# Modules

These are the modules Volant runs. A playbook naming any other module is refused when it is loaded, before the first task, the way Ansible refuses a module it cannot resolve. Everything else waits on the warm Python path.

## On the agent

The agent runs these on the host, without Python. The arguments column lists what each module reads. The list under the table names the arguments Ansible has and this release refuses.

| Module | Free-form arguments | Arguments | What it does |
|---|---|---|---|
| `command` | yes | `argv`, `chdir`, `cmd`, `creates`, `removes`, `stdin`, `stdin_add_newline`, `strip_empty_ends` | Run a program directly, without a shell. |
| `raw` | yes | `executable` | Run a command line through the remote shell, with no module machinery around it. |
| `shell` | yes | `argv`, `chdir`, `cmd`, `creates`, `executable`, `removes`, `stdin`, `stdin_add_newline`, `strip_empty_ends` | Run a command line through a shell, `sh` unless `executable` names another. |

Volant refuses these before the run reaches a host:

- `command`: `expand_argument_vars: true`
- `shell`: `expand_argument_vars: true`

## On the controller

The controller runs these itself, so they need no connection to the host.

| Module | Free-form arguments | What it does |
|---|---|---|
| `debug` | no | Print a message or the value of a variable. |
| `include_vars` | yes | Read a file of variables and set them on the host, for the rest of the run. |
| `set_fact` | no | Set facts for a host, for the rest of the run. |
| `validate_argument_spec` | no | Check a role's arguments against the specification in `meta/argument_specs.yml`. |
