# Architecture

This document is the map for contributors. It describes the shape of the code, not every detail.

## Two binaries

- `crates/volant`: the controller. It reads `ansible.cfg`, inventories, playbooks, roles and collections, resolves variables, renders templates, compiles each play into batches of tasks per host, and talks to agents.
- `crates/volant-agent`: a static binary uploaded once per managed host and cached there. It receives batches of tasks, runs them, and streams results back. It contains the native modules, the facts collector, and a supervisor for a warm Python interpreter that runs unmodified Ansible modules.

## How a playbook runs

1. The controller loads configuration, inventory and playbooks and resolves variables without connecting anywhere.
2. Each play becomes, per host, a sequence of tasks. The controller places a boundary before any task whose inputs depend on a value produced at runtime (`register`, `set_fact`, gathered facts, another host's variables). Tasks between two boundaries form a batch. Each host renders and batches its own tasks, so hosts can be at different points in the same play at the same time.
3. The controller opens one connection per host (SSH by default), uploads the agent if it is not cached, and sends batches. Templating happens on the controller; execution happens in the agent.
4. Results stream back task by task. The controller applies `register`, `set_fact`, `changed_when`, `failed_when`, queues handlers and renders output.

## Where things live

- `crates/volant-protocol`: frames and messages shared by both binaries. Changing a message means bumping `PROTOCOL_VERSION`.
- `crates/volant/src/inventory.rs`, `playbook.rs`: loading Ansible content.
- `crates/volant/src/yaml.rs`: YAML parsing on saphyr, with PyYAML's scalar typing and Ansible's YAML tags.
- `crates/volant/src/config.rs`: the `ansible.cfg` settings this release reads.
- `crates/volant/src/vars.rs`: variable sources and Ansible's precedence, plus the magic variables.
- `crates/volant/src/template/`: the Jinja2 templar, its Ansible filters, tests and lookups.
- `crates/volant/src/transport.rs`, `agent.rs`: reaching a host and talking to its agent.
- `crates/volant/src/executor.rs`: the linear strategy and the task-by-task coordinator.
- `crates/volant/src/render.rs`, `stats.rs`: console output, recap, exit codes.
- `crates/volant-agent/src/runner.rs`: batch execution. `modules/`: native modules.
- `docs/adr/`: decisions that affect users, with their reasoning.
