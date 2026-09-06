# Native modules

Modules the agent runs without Python. Every other module runs through the warm Python path once it exists.

| Module | Free-form arguments | What it does |
|---|---|---|
| `command` | yes | Run a program directly, without a shell. |
| `raw` | yes | Run a command line through the remote shell, with no module machinery around it. |
| `shell` | yes | Run a command line through `sh -c`. |
