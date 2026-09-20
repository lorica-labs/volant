# Architecture

This document is the map for contributors. It describes the shape of the code, not every detail.

## Two binaries

- `crates/volant`: the controller. It reads `ansible.cfg`, inventories, playbooks and roles, resolves variables, renders templates, compiles each play into batches of tasks per host, and talks to agents.
- `crates/volant-agent`: a static binary uploaded once per managed host and cached there. It receives batches of tasks, runs them, and streams results back. It contains the native modules and nothing else yet.

## How a playbook runs

1. The controller loads configuration, inventory and playbooks and resolves variables without connecting anywhere.
2. Each play becomes, per host, a sequence of tasks. By default the controller places a boundary in front of every task, the way Ansible's `linear` strategy does: nobody starts task two until everybody has finished task one. Tasks between two boundaries form a batch, so the default batch is one task wide. With [cross-host batching](docs/src/batching.md) turned on, a boundary is placed only before a task whose inputs depend on a value produced at runtime (`register`, `set_fact`, another host's variables), each host renders and batches its own tasks, and hosts can then be at different points in the same play at the same time.
3. The controller opens one connection per host (SSH by default), uploads the agent if it is not cached, and sends batches. Templating happens on the controller; execution happens in the agent.
4. Results stream back task by task. The controller applies `register`, `set_fact`, `changed_when`, `failed_when`, queues handlers and renders output.

## Compiling a play into steps

A play is written as a tree and run as a sequence, so it is flattened once, before the first connection. `pre_tasks`, the roles, `tasks` and `post_tasks` become one numbered list of steps, with a handler flush point behind each of the three sections that laid a step out. What the tree said about grouping survives as a span per block plus the section each step sits in, and a jump table over those spans decides where a host goes after a step succeeds, after it fails, and after it is stepped over. Every host walks the same numbered list, so the coordinator can talk about step 7 to all of them at once.

The dangerous half of a flattening is the step nobody runs, since an index skipped quietly is a run that reports success having done less than the playbook asked for. Nothing in the compiler invents an index, and the driver tells the coordinator about every index it steps over, so a step left out is one the recap can still account for.

Two kinds of step have successors nobody knows at compile time: a handler flush, which depends on what the run notified, and a dynamic include, whose file is named by variables the host has not rendered yet. Both are splice points. The splice inserts the new steps where the statement stood and grows the spans that already covered it, and the coordinator publishes a splice at every splice point, an empty one included. A host waits there until that publication arrives, because a host that walks past an insert holds an index the insert has moved, and the step behind it then runs twice.

The pre-flight runs once, over that compilation, so it sees only the steps the compiler laid out. Whatever a dynamic include splices in during the run was not there to be checked, and a module or keyword this release refuses can therefore be reached after the play has started rather than before the first connection. A statically imported file is compiled with the rest and is checked with it.

## Reaching a host over SSH

1. One `ssh` call asks the cached agent for its version, or reports the machine architecture and a bootstrap exit code saying what is missing.
2. If that version is not the controller's own, a second call pipes the matching static agent in on stdin: free space checked, written under a temporary name, byte count verified, renamed into place, older versions removed.
3. A third call opens the long-lived link: `ssh` runs the cached agent, and the protocol speaks over that process's stdin and stdout for the rest of the run.
4. A link belongs to one (host, target user) pair. `become` opens a second one, where the remote command is the agent under `sudo`; unescalated tasks keep the first. A `sudo` probe settles which form of the invocation to use before the link opens.

## Where things live

- `crates/volant-protocol`: frames and messages shared by both binaries. Changing a message means bumping `PROTOCOL_VERSION`.
- `crates/volant/src/inventory.rs`, `playbook.rs`: loading Ansible content.
- `crates/volant/src/yaml.rs`: YAML parsing on saphyr, with PyYAML's scalar typing and Ansible's YAML tags.
- `crates/volant/src/config.rs`: the `ansible.cfg` settings this release reads.
- `crates/volant/src/vars.rs`: variable sources and Ansible's precedence, plus the magic variables.
- `crates/volant/src/template/`: the Jinja2 templar, its Ansible filters, tests and lookups.
- `crates/volant/src/transport.rs`, `agent.rs`: reaching a host and talking to its agent.
- `crates/volant/src/keywords.rs`: every keyword ansible-core knows, and whether this release runs it or refuses it. `docs/src/keywords.md` is generated from it.
- `crates/volant/src/preflight.rs`: what a run refuses before it touches a host.
- `crates/volant/src/compile.rs`, `roles.rs`: a play flattened into steps, and the roles spliced into them.
- `crates/volant/src/listing.rs`: `--list-tasks`, `--list-tags`, `--list-hosts` and `--syntax-check`.
- `crates/volant/src/executor/`: the linear strategy, in seven modules.
  - `mod.rs`: the run's settings, the connections kept between plays, and the entry point the command line calls.
  - `coordinator.rs`: the serialised view of a batch's progress, and everything that reaches the terminal.
  - `driver.rs`: one host's run through a play, from its first step to the moment it leaves the batch.
  - `prepare.rs`: rendering one task for one host, with its variables, escalation, environment and loop items.
  - `run.rs`: running one task and judging its result, on the controller as well as on the agent.
  - `report.rs`: turning one task's results into the events the coordinator shows.
  - `include.rs`: resolving a dynamic `include_tasks`, `import_tasks` or `include_role` for one host.
- `crates/volant/src/render.rs`, `stats.rs`: console output, recap, exit codes.
- `crates/volant-agent/src/runner.rs`: batch execution. `modules/`: native modules.
- `docs/adr/`: decisions that affect users, with their reasoning.
