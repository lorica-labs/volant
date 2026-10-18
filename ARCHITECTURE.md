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
| `cli/` | The `playbook` command: `args` holds its arguments, `announce` the version line and option dumps, `run` the run itself, `hosts` the limit, tags and ordering, `passwords` the password files and prompts. |
| `config.rs` | The `ansible.cfg` settings this release reads. |
| `inventory/` | Inventories from every source plugin, with the group graph in `graph.rs` and host patterns, globs and subscripts in `patterns.rs`. |
| `playbook/`, `yaml.rs` | Loading plays: `model` holds the parsed shape, `load`, `play`, `task` and `fields` parse it, `keywords` the keyword tables. YAML on saphyr, with PyYAML's scalar typing and Ansible's tags. |
| `vars/` | Variable sources and Ansible's precedence: `store` builds the `VarStore`, `layers` merges a host's view, `facts` holds gathered and cached facts, `magic` the magic variables, `load` reads variable files. |
| `template/` | The Jinja2 templar, with Ansible's filters, tests and lookups. `templar.rs` is `Templar`, `context.rs` the variables a render reads and their trust, `env.rs` the minijinja environment and the registration helpers, `lookups/` the lookup plugins, with `lookup` and its dispatch in `mod.rs` and one file each for the basic ones, `template`, `fileglob` and `first_found`. `vm.rs` the bytecode rewrite and error conversion. |
| `template/` | The Jinja2 templar, with Ansible's filters, tests and lookups. `templar.rs` is `Templar`, `context.rs` the variables a render reads and their trust, `env.rs` the minijinja environment and the registration helpers, `vm.rs` the bytecode rewrite and error conversion, `test_plugins.rs` the Jinja tests Ansible ships. |
| `keywords.rs` | Every keyword ansible-core knows, and whether this release runs or refuses it. The keyword reference page is generated from it. |
| `action_plugins/` | The action plugins this release runs itself (`package`, `service`), and the builtin modules whose plugin it does not have yet and refuses. |
| `preflight.rs` | What a run refuses before it touches a host. |
| `compile/`, `roles.rs` | A play flattened into steps (`builder` walks the tree, `model` and `compiled` hold the steps, `nav` finds the next one, `include` grafts dynamic includes), and roles spliced into them. |
| `listing.rs` | `--list-tasks`, `--list-tags`, `--list-hosts` and `--syntax-check`. |
| `transport/`, `agent.rs` | Reaching a host, finding the agent binaries, and talking to a running agent. `transport` holds the connection settings, the `ssh` command lines and shared connections, the agent bootstrap, and `escalation/` for `sudo` and `su`. |
| `agent/embedded.rs`, `build.rs` | The agents built into a release controller, and their extraction to a cache directory on first use. |
| `python/`, `python_helper.py` | The Python helper that builds module payloads: `types` holds the facts and payload shapes, `reach` what a run can reach, `union_build` builds the union, `builder` talks to the helper, `action_helper` runs action plugins. |
| `render/`, `stats.rs` | Console output, the recap, and exit codes. `render/` keeps the renderer in `mod.rs`, one task's result line in `result.rs`, the error blocks in `error.rs` and Ansible's JSON text in `recap_json.rs`. |

### Executor, `crates/volant/src/executor/`

| File | What it holds |
|---|---|
| `mod.rs` | The run's settings, the connections kept between plays, and the entry point the command line calls. |
| `coordinator.rs` | The serialized view of a batch's progress, and everything that reaches the terminal. |
| `driver/` | One host's run through a play, from its first step to the moment it leaves. `mod.rs` holds `drive_host`, `boundaries.rs` the scan that decides where the hosts meet, `waits.rs` the waits on the other hosts, `unreachable.rs` the `ignore_unreachable` steps, `timings.rs` the profile records of a link. |
| `prepare.rs` | Rendering one task for one host: variables, escalation, environment and loop items. |
| `run.rs` | Running one task and judging its result, on the controller or on the agent. |
| `report.rs` | Turning one task's results into the events the coordinator shows. |
| `include.rs` | Resolving a dynamic `include_tasks` or `include_role` for one host. |

### Agent, `crates/volant-agent/src/`

| Path | What it holds |
|---|---|
| `runner.rs` | Running a batch of tasks, with fail-fast and cancellation. |
| `modules/` | The native modules. |
| `modules/command/` | `command`, `shell` and `raw`: the run, the process and its pipes, the argument expansion and the failure results. |
| `python/` | The warm Python server and its supervisor: the server process, the pool that keeps one per interpreter and payload, and the frames it speaks. |
| `natives/common/` | What every native shares with the reference's module machinery: arguments, accounts, modes, file attributes, running programs and backups. |
| `natives/service_facts/` | The native `service_facts` for systemd hosts: the run, the SysV listing and the systemd units. |
| `natives/lineinfile/` | The native `lineinfile`: the request, the edit in memory, the `regexp` translation and the write. |
| `natives/copy/` | The native `copy`: the request, the source and destination sums with the MD5 it carries, the mount check, and the move. |
| `natives/runtime/` | What the natives that run programs share: the task's clock, the reasons an answer stops, the root a collector reads under, program runs, and Python's string rules. |
| `natives/setup/` | The native `setup`: one file per collector, plus the request, the collection run and the `fnmatch` filter. |
| `natives/systemd/` | The native `systemd`: the answer, the request and its options, the `systemctl` host, and the reading of `systemctl show` and init scripts. |
| `natives/file/` | The native `file`: the answer and the states it ensures, and the check before a tree is removed. |
| `natives/package_facts/` | The native `package_facts` for apt: the run and its arguments, dpkg's status, the configured sources, and the scan of the lists. |
| `blobs/` | The cache of module payloads, addressed by hash: reading the controller's messages, the staged files of a connection, and the filesystem checks. |
| `interpreter.rs` | Finding the host's Python interpreters, in the order Ansible prefers them. |

### Protocol, `crates/volant-protocol/src/`

| File | What it holds |
|---|---|
| `frame.rs`, `messages.rs` | The wire format and the messages. |
| `modules/` | The native and controller modules and the arguments each one reads, the builtin module list, the action plugins and the generated module reference page. |

### Documentation and tests

| Path | What it holds |
|---|---|
| `docs/` | The documentation site, built with Starlight. Pages live under `docs/src/content/docs/`. |
| `docs/src/content/docs/decisions/` | Decision records that affect users, with their reasoning. |
| `crates/volant/tests/golden/` | Output recorded from ansible-core 2.19.12, which the tests compare against. |
| `crates/volant/tests/fixtures/` | Playbooks the integration tests run. |
