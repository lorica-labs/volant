# Architecture

This is the map of the source tree for contributors. It says where things live, not how every detail works. The [architecture page](https://volant.sh/internals/architecture/) of the documentation explains the design with diagrams, and the [decision records](https://volant.sh/decisions/) explain why it is built this way.

## Three crates

| Crate | Role |
|---|---|
| `crates/volant` | The controller. It reads `ansible.cfg`, inventories, playbooks and roles, resolves variables, renders templates, compiles each play into steps, and drives the agents. |
| `crates/volant-agent` | A static binary uploaded once to each managed host and cached there. It runs batches of tasks, natively for `command`, `shell` and `raw`, and through a warm Python server for everything else. |
| `crates/volant-protocol` | The frames and messages both binaries share. Changing a message means bumping `PROTOCOL_VERSION`. |

## How a playbook runs

1. The controller loads configuration, inventory and playbooks, and resolves variables without connecting anywhere.
2. Each play is compiled into one numbered list of steps: `pre_tasks`, roles, `tasks` and `post_tasks`, with a handler flush point after each of the three sections. Blocks survive as spans over that list, and a jump table over the spans says where a host goes after a step succeeds, fails or is skipped. Every host walks the same list, so the coordinator can talk about step 7 to all of them at once.
3. The pre-flight refuses, by name, everything the loader accepted and this release cannot execute. It runs over the playbook as written and again over the compiled steps.
4. The executor starts one driver per host, bounded by `forks`. By default a synchronization point sits in front of every task, as in Ansible's `linear` strategy. With `[volant] batching` on, only a `run_once` task, a task whose text names `hostvars`, `ansible_play_hosts` or `ansible_play_batch`, and a splice point are synchronization points, and each host batches the tasks in between.
5. The controller opens one connection per host (SSH by default), uploads the agent if it is not cached, and sends batches. Templating happens on the controller, execution in the agent.
6. Results stream back task by task. The controller applies `register`, `set_fact`, `changed_when` and `failed_when`, queues handlers, and renders the output.

## Splice points

Two kinds of step have followers nobody knows at compile time: a handler flush, which depends on what the run notified, and a dynamic include, whose file name depends on variables a host has not rendered yet. Both are splice points. The splice inserts the new steps where the statement stood and grows the spans that covered it. The coordinator publishes a splice at every splice point, an empty one included, and a host waits there until it arrives: a host that walked past an insert would hold an index the insert has moved, and would run the step behind it twice.

Skipping a step quietly would be a run that reports success after doing less than the playbook asked. Nothing in the compiler invents an index, and the driver tells the coordinator about every index it steps over, so the recap can account for every step.

What a dynamic include reads goes through the pre-flight before it is spliced in. By then earlier tasks have run, so a refusal fails the include statement for the host that reached it, and a `rescue` around the statement can catch it.

## Reaching a host over SSH

1. One `ssh` call asks the cached agent for its version, or reports the machine architecture and a bootstrap exit code saying what is missing.
2. If that version is not the controller's own, a second call pipes the matching static agent in on stdin: free space checked, written under a temporary name, byte count verified, renamed into place, older versions removed.
3. A third call opens the long-lived link: `ssh` runs the cached agent, and the protocol runs over that process's stdin and stdout for the rest of the run.
4. A link belongs to one (host, target user) pair. `become` opens a second one, where the remote command is the agent under `sudo`, and keeps it until the end of the play. A `sudo` probe decides which form of the invocation to use before the link opens.

## The Python path

The controller keeps one Python helper process for the whole run. It uses ansible-core to build the payload of every module the run needs, and merges them into one zip named by its BLAKE3 hash. The agent caches that zip, verifies its hash, and starts one Python server per target user. The server loads the shared `module_utils` once and forks a child for each task.

## Where things live

### Controller, `crates/volant/src/`

| Path | What it holds |
|---|---|
| `cli.rs` | The `playbook` command and its arguments. |
| `config.rs` | The `ansible.cfg` settings this release reads. |
| `inventory.rs` | INI inventories and host patterns. |
| `playbook.rs`, `yaml.rs` | Loading plays; YAML on saphyr, with PyYAML's scalar typing and Ansible's tags. |
| `vars.rs` | Variable sources, Ansible's precedence, and the magic variables. |
| `template/` | The Jinja2 templar, with Ansible's filters, tests and lookups. |
| `keywords.rs` | Every keyword ansible-core knows, and whether this release runs or refuses it. The keyword reference page is generated from it. |
| `action_plugins/` | The action plugins this release runs itself (`package`, `service`), and the builtin modules whose plugin it does not have yet and refuses. |
| `preflight.rs` | What a run refuses before it touches a host. |
| `compile.rs`, `roles.rs` | A play flattened into steps, and roles spliced into them. |
| `listing.rs` | `--list-tasks`, `--list-tags`, `--list-hosts` and `--syntax-check`. |
| `transport.rs`, `agent.rs` | Reaching a host, finding the agent binaries, and talking to a running agent. |
| `python.rs`, `python_helper.py` | The Python helper that builds module payloads. |
| `render.rs`, `stats.rs` | Console output, the recap, and exit codes. |

### Executor, `crates/volant/src/executor/`

| File | What it holds |
|---|---|
| `mod.rs` | The run's settings, the connections kept between plays, and the entry point the command line calls. |
| `coordinator.rs` | The serialized view of a batch's progress, and everything that reaches the terminal. |
| `driver.rs` | One host's run through a play, from its first step to the moment it leaves. |
| `prepare.rs` | Rendering one task for one host: variables, escalation, environment and loop items. |
| `run.rs` | Running one task and judging its result, on the controller or on the agent. |
| `report.rs` | Turning one task's results into the events the coordinator shows. |
| `include.rs` | Resolving a dynamic `include_tasks` or `include_role` for one host. |

### Agent, `crates/volant-agent/src/`

| Path | What it holds |
|---|---|
| `runner.rs` | Running a batch of tasks, with fail-fast and cancellation. |
| `modules/` | The native modules. |
| `python/` | The warm Python server and its supervisor. |
| `blobs.rs` | The cache of module payloads, addressed by hash. |
| `interpreter.rs` | Finding the host's Python interpreters, in the order Ansible prefers them. |

### Protocol, `crates/volant-protocol/src/`

| File | What it holds |
|---|---|
| `frame.rs`, `messages.rs` | The wire format and the messages. |
| `modules.rs` | The native and controller modules, and the arguments each one reads. The module reference page is generated from it. |

### Documentation and tests

| Path | What it holds |
|---|---|
| `docs/` | The documentation site, built with Starlight. Pages live under `docs/src/content/docs/`. |
| `docs/src/content/docs/decisions/` | Decision records that affect users, with their reasoning. |
| `crates/volant/tests/golden/` | Output recorded from ansible-core 2.19.12, which the tests compare against. |
| `crates/volant/tests/fixtures/` | Playbooks the integration tests run. |
