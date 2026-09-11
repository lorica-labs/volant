# Modules

These are the modules Volant runs. A playbook naming any other module is refused when it is loaded, before the first task, the way Ansible refuses a module it cannot resolve. Everything else waits on the warm Python path.

## On the agent

The agent runs these on the host, without Python.

| Module | Free-form arguments | What it does |
|---|---|---|
| `command` | yes | Run a program directly, without a shell. |
| `raw` | yes | Run a command line through the remote shell, with no module machinery around it. |
| `shell` | yes | Run a command line through `sh -c`. |

## On the controller

The controller runs these itself, so they need no connection to the host.

| Module | Free-form arguments | What it does |
|---|---|---|
| `debug` | no | Print a message or the value of a variable. |
| `set_fact` | no | Set facts for a host, for the rest of the run. |
| `validate_argument_spec` | no | Check a role's arguments against the specification in `meta/argument_specs.yml`. |
