# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.1.0-alpha.8](https://github.com/lorica-labs/volant/compare/v0.1.0-alpha.7...v0.1.0-alpha.8) - 2026-09-26

### Added

- *(cli)* add --profile and a switch for native modules ([#236](https://github.com/lorica-labs/volant/pull/236))
- *(agent)* run a native module and hand the task back to python ([#230](https://github.com/lorica-labs/volant/pull/230))
- *(template)* add the extract filter ([#222](https://github.com/lorica-labs/volant/pull/222))
- *(reboot)* reboot the host and wait for a new boot id ([#219](https://github.com/lorica-labs/volant/pull/219))
- loop with lookups, set the run tags, retry an action plugin ([#216](https://github.com/lorica-labs/volant/pull/216))
- *(fetch)* write a host's file under dest and nowhere else ([#215](https://github.com/lorica-labs/volant/pull/215))
- *(dnf)* run dnf through its action plugin ([#213](https://github.com/lorica-labs/volant/pull/213))
- *(python)* run modules from installed collections ([#211](https://github.com/lorica-labs/volant/pull/211))
- *(template)* add fileglob, ipwrap and the builtin aliases ([#210](https://github.com/lorica-labs/volant/pull/210))
- embed the agents in the release controller ([#206](https://github.com/lorica-labs/volant/pull/206))
- *(template)* add the remaining filters and two lookups ([#193](https://github.com/lorica-labs/volant/pull/193))
- *(actions)* unarchive a local or remote archive ([#194](https://github.com/lorica-labs/volant/pull/194))
- *(actions)* render a template on the controller and copy it ([#192](https://github.com/lorica-labs/volant/pull/192))
- *(actions)* copy a file, sending it only when it differs ([#190](https://github.com/lorica-labs/volant/pull/190))
- *(template)* add python methods, result and version tests ([#189](https://github.com/lorica-labs/volant/pull/189))
- *(template)* render a template file once, as the module does ([#188](https://github.com/lorica-labs/volant/pull/188))
- *(actions)* run package and service through their manager ([#186](https://github.com/lorica-labs/volant/pull/186))
- *(protocol)* stage files the agent hands to a module ([#183](https://github.com/lorica-labs/volant/pull/183))
- *(agent)* collect cpu, memory and routes natively ([#242](https://github.com/lorica-labs/volant/pull/242))
- *(agent)* collect the minimal facts natively ([#237](https://github.com/lorica-labs/volant/pull/237))

### Changed

- *(vars)* read host variables through shared layers ([#239](https://github.com/lorica-labs/volant/pull/239))
- *(transport)* share one ssh connection per inventory host ([#238](https://github.com/lorica-labs/volant/pull/238))
- *(controller)* cache the python module union between runs ([#235](https://github.com/lorica-labs/volant/pull/235))

### Fixed

- *(template)* fail a concatenation holding an undefined value ([#234](https://github.com/lorica-labs/volant/pull/234))
- *(template)* fail a text filter over a container holding undefined ([#229](https://github.com/lorica-labs/volant/pull/229))
- *(template)* propagate undefined through filters like ansible-core ([#228](https://github.com/lorica-labs/volant/pull/228))
- *(executor)* count a looped include whose items all skip ([#227](https://github.com/lorica-labs/volant/pull/227))
- *(executor)* read when before failing on an undefined loop ([#223](https://github.com/lorica-labs/volant/pull/223))
- *(transport)* reuse a cached agent only if it is the same binary ([#221](https://github.com/lorica-labs/volant/pull/221))
- *(render)* print a loop's skipping line when every item skipped ([#202](https://github.com/lorica-labs/volant/pull/202))
- *(executor)* set the search path before a task's own vars resolve ([#200](https://github.com/lorica-labs/volant/pull/200))
- *(include)* hand a role's keywords to the tasks it includes ([#199](https://github.com/lorica-labs/volant/pull/199))
- *(vars)* load an empty vars file and render ignore_errors late ([#196](https://github.com/lorica-labs/volant/pull/196))
- *(agent)* keep a staged file to the connection that sent it ([#191](https://github.com/lorica-labs/volant/pull/191))

### Added

- Run `copy` as an action plugin: `stat` the destination, then `file` or a staged `copy`, sending nothing when the file already matches.
- Run `package` as an action plugin: pick the module from the host's package manager, gathered with a filtered `setup` when it is not already a fact.
- Run `service` as an action plugin: pick the module from the host's init system (`use:`, then a fact, then a filtered `setup`), falling back to the `service` module for a name none of the known backends carries rather than failing the task.
- Run `template` as an action plugin: render the file on the controller once, then hand it to the same path `copy` uses.
- Run `unarchive` as an action plugin: extract a controller archive or one already on the host, skipping the task when `creates:` already exists.
- Add the Jinja filters, tests, methods and lookups the template engine was missing: `b64decode`, `b64encode`, `comment`, `difference`, `intersect`, `union`, `flatten`, `from_yaml`, `to_yaml`, `to_nice_yaml`, `to_uuid`, `type_debug`, `quote` and `regex_escape`; the `changed`, `failed`, `succeeded`, `skipped` and `version` tests; a handful of Python methods on strings and mappings, through `minijinja-contrib`; and the `first_found` and `template` lookups.
- Run `reboot` as an action plugin: shut the host down, then reconnect until it answers with a new boot id and its test command succeeds.
- Run `with_first_found` and `with_fileglob` loops, set `ansible_run_tags` and `ansible_skip_tags` from `--tags` and `--skip-tags`, and retry a task backed by an action plugin under `until`, running its whole sequence of sub-tasks again on each attempt. `retries: R` is now up to R + 1 runs, as in the reference. ([#216](https://github.com/lorica-labs/volant/pull/216))
- Run `fetch` as an action plugin, writing a host's file onto the controller under `dest` and nowhere else, refusing a path that would land outside it. ([#215](https://github.com/lorica-labs/volant/pull/215))
- Run `dnf` as an action plugin, picking the `dnf` or `dnf5` module from the host's package manager. ([#213](https://github.com/lorica-labs/volant/pull/213))
- Run modules from installed collections, such as `ansible.posix.sysctl` or `community.general.ufw`, on the warm Python path. The controller's ansible-core says what each name is, and a missing collection, a collection's action plugin or a module that cannot be built is refused before the first connection. Volant never installs a collection. ([#211](https://github.com/lorica-labs/volant/pull/211))
- Add the `fileglob` lookup and the `ansible.utils.ipwrap` filter, and answer every filter and test under its `ansible.builtin.*` name too. ([#210](https://github.com/lorica-labs/volant/pull/210))
- An install script: `curl -fsSL https://volant.sh/install.sh | sh` installs the newest release, controller and agents together, after checking its checksum.
- The release controller carries its agents. `cargo binstall volant`, the shell installer and the release archives give a `volant` that runs local and SSH tasks with nothing else installed. On first use the agents are extracted to `~/.cache/volant/agents/<version>`, or under `$XDG_CACHE_HOME`.

### Changed

- Agents are looked up in `VOLANT_AGENT_DIR` first, then among the agents built into the controller, then beside the executable.
- Release archives no longer contain the agents. They are still published on their own, in the `volant-agent-*` archives.
- A `copy`, `template` or `unarchive` task whose `src` was named by a managed host is now rejected before the file is looked up, rather than sent the way the reference sends it: a host that controls a command's output or a fact could otherwise have any file the operator can read copied to it.
- The [documentation site](https://volant.sh/) moves to volant.sh and is rebuilt with sections, search and diagrams. It adds pages on installation, compatibility, the command line, configuration, inventories and exit codes.

### Fixed

- A cached agent on a host is reused only if it is the exact binary the controller would upload. A different build that reported the same version used to be run as the agent. Reusing a cached agent now needs an agent for the host's architecture on the controller; the release controller embeds both.
- A controller started through a symbolic link finds its agents next to the file the link points at. On macOS it used to look in the directory of the link.
- A file staged for a module belongs to the connection that sent it. Two links to one host no longer consume each other's files, and a staged file left by a cancelled batch, a lost link or a killed agent is cleaned up. ([#191](https://github.com/lorica-labs/volant/pull/191))
- A vars file holding only `---` and a comment loads as an empty mapping, a templated `ignore_errors` is rendered when the task runs, `ansible_search_path` is set for every task and `role_path` for a role's, and Python module results get `stdout_lines` and `stderr_lines`. ([#196](https://github.com/lorica-labs/volant/pull/196))
- Tasks brought in by `include_tasks` or `include_role` inherit the tags and other keywords of the play, role and blocks around the statement. Under `--tags`, a tagged role's included files used to run nothing while the run exited 0. The Python union also carries the modules of every file of the roles in play, and a failed module's `exception` no longer shows as a raw object. ([#199](https://github.com/lorica-labs/volant/pull/199))
- A task's own `vars:` built from a lookup that needs the role's search path, such as `lookup('template')`, now resolve. ([#200](https://github.com/lorica-labs/volant/pull/200))
- A loop whose every item was skipped prints the task's own `skipping:` line after the item lines, as ansible-core does. ([#202](https://github.com/lorica-labs/volant/pull/202))
- A `notify` naming no handler in a dynamically included file ends the run with exit code 1, as ansible-core does. `fatal:` lines no longer show `"failed": true`, module warnings are printed as `[WARNING]:` lines, and an interrupted `pause` ends the run without counting a failure. ([#204](https://github.com/lorica-labs/volant/pull/204))

## [0.1.0-alpha.7](https://github.com/lorica-labs/volant/compare/v0.1.0-alpha.6...v0.1.0-alpha.7) - 2026-09-22

### Security

- A managed host can no longer set its own connection settings through the facts a module returns. Until this release, a fact module's `ansible_facts` reached the host's variables unfiltered, so a host could change the address, user and ssh arguments of its own next task, including `-oProxyCommand`, which runs a command on the controller. It could also pick the Python interpreter its next module ran under, switch the connection to `local` to move its next task onto the controller, and move `ansible_remote_tmp` so that a planted agent ran as root on the next `become` task. Volant now strips the names ansible-core's `clean_facts()` strips, plus `ansible_remote_tmp`, which ansible-core does not use this way, and prints the same warning. ([#181](https://github.com/lorica-labs/volant/pull/181))

### Added

- Run ansible-core's Python modules on managed hosts. The controller builds one content-addressed zip of the modules a run needs and their `module_utils`, and sends it to each host once. A Python server in the agent loads it once and forks a child per task, so running a module costs tens of milliseconds rather than starting a new interpreter for each one. The host needs a Python interpreter but not ansible-core; the controller needs ansible-core. Modules the reference runs through an action plugin, such as `copy`, `template`, `service` and `package`, are still refused before the first connection. ([#158](https://github.com/lorica-labs/volant/pull/158), [#159](https://github.com/lorica-labs/volant/pull/159), [#163](https://github.com/lorica-labs/volant/pull/163), [#166](https://github.com/lorica-labs/volant/pull/166), [#171](https://github.com/lorica-labs/volant/pull/171))
- Gather facts with ansible-core's own `setup` module when a play asks for them. Gathered facts are data from the host and are never rendered as templates. ([#173](https://github.com/lorica-labs/volant/pull/173))
- Run `assert`, `fail` and `pause` on the controller. ([#179](https://github.com/lorica-labs/volant/pull/179))

### Changed

- A play that does not write `gather_facts: false` now gathers facts, so it needs ansible-core on the controller and is refused before the first connection without it. ([#173](https://github.com/lorica-labs/volant/pull/173))
- Gathered facts now rank above the inventory and below the play's, role's and task's own variables, as in ansible-core. A playbook that sets a variable with the same name as a gathered fact now reads its own value. ([#181](https://github.com/lorica-labs/volant/pull/181))
- An escalated connection is kept for the whole play instead of being rebuilt at every task. On a thirty-five task playbook against one host this took the run from 872 ms to 226 ms per task. A run can now hold two connections per host, so a controller limited to 1024 open files reaches that limit at about 169 escalating hosts. ([#178](https://github.com/lorica-labs/volant/pull/178))

### Fixed

- `ansible_facts[...]` uses the same keys as ansible-core, without an `ansible_` prefix, and a fact a module returns without the prefix keeps its name. ([#181](https://github.com/lorica-labs/volant/pull/181))
- A command skipped because of `creates:` or `removes:` is reported as `ok`, as in ansible-core, rather than as skipped. ([#181](https://github.com/lorica-labs/volant/pull/181))
- A registered result carries `changed` and `failed` the way ansible-core's does, and a result such as `rc: "2"` counts as a failure as it does there. ([#181](https://github.com/lorica-labs/volant/pull/181))
- The `timeout:` keyword applies to tasks that run on the controller. ([#179](https://github.com/lorica-labs/volant/pull/179))

## [0.1.0-alpha.6](https://github.com/lorica-labs/volant/compare/v0.1.0-alpha.5...v0.1.0-alpha.6) - 2026-09-20

### Added

- Every release archive now carries the two Linux agents, so one download is enough for a first run over SSH. ([#147](https://github.com/lorica-labs/volant/pull/147))
- The documentation lists which arguments each native module reads, and the pre-flight refuses the ones it does not. ([#145](https://github.com/lorica-labs/volant/pull/145))

### Changed

- Hosts now meet in front of every task, as Ansible's `linear` strategy promises. The previous behavior, where each host ran ahead through tasks that did not read another host's state, is available behind the `[volant] batching` option. ([#140](https://github.com/lorica-labs/volant/pull/140))
- Resolving variables on large inventories is faster: inventory-wide values are built once per batch and shared across hosts. ([#134](https://github.com/lorica-labs/volant/pull/134))

### Fixed

- A module result is data and is never rendered again as a template on the controller. ([#139](https://github.com/lorica-labs/volant/pull/139))
- The connection settings come from a host's effective variables, including `group_vars`, task `vars`, `set_fact` and `--extra-vars`, not only from the inventory. ([#141](https://github.com/lorica-labs/volant/pull/141))
- `creates` and `removes` are resolved in the directory the command runs in. ([#142](https://github.com/lorica-labs/volant/pull/142))
- A task's timeout covers the child process, its output streams and the write to its standard input. ([#144](https://github.com/lorica-labs/volant/pull/144)), ([#154](https://github.com/lorica-labs/volant/pull/154))
- A `remote_tmp` whose `~` part is not a user name is refused. ([#146](https://github.com/lorica-labs/volant/pull/146))
- Every event a task produces is shown, through the same output renderer as the rest of the run. ([#143](https://github.com/lorica-labs/volant/pull/143))
- What a dynamic include brings in is checked by the pre-flight before it runs. ([#155](https://github.com/lorica-labs/volant/pull/155))

## [0.1.0-alpha.5](https://github.com/lorica-labs/volant/compare/v0.1.0-alpha.4...v0.1.0-alpha.5) - 2026-09-16

### Added

- The whole ansible-core 2.19 grammar loads, and whatever this release cannot run is refused by name before the first connection. ([#98](https://github.com/lorica-labs/volant/pull/98))
- Plays compile into a flat list of steps, with blocks kept as spans. ([#101](https://github.com/lorica-labs/volant/pull/101))
- Roles load from the standard paths, with dependencies, argument specs and `import_tasks`. ([#102](https://github.com/lorica-labs/volant/pull/102))
- `--tags`, `--skip-tags` and the listing commands, with output identical to Ansible's. ([#103](https://github.com/lorica-labs/volant/pull/103))
- Blocks with `rescue` and `always`, and the `ansible_failed_task` and `ansible_failed_result` facts. ([#104](https://github.com/lorica-labs/volant/pull/104))
- Handlers, run at every flush point. ([#105](https://github.com/lorica-labs/volant/pull/105))
- `until`, `retries` and `delay`, `no_log`, and `environment`. ([#107](https://github.com/lorica-labs/volant/pull/107))
- `serial` batches, with the run stopping when a batch loses every host. ([#108](https://github.com/lorica-labs/volant/pull/108))
- `include_tasks`, `include_role` and `include_vars`, resolved while the play runs. ([#109](https://github.com/lorica-labs/volant/pull/109))
- `run_once` and `delegate_to`, with the delegated task running over the delegate's own connection. ([#115](https://github.com/lorica-labs/volant/pull/115))

### Changed

- Every host reads one shared `hostvars` map instead of its own copy. ([#116](https://github.com/lorica-labs/volant/pull/116))

### Fixed

- The host that runs a `run_once` task is elected once for the whole batch. ([#118](https://github.com/lorica-labs/volant/pull/118))

## [0.1.0-alpha.4](https://github.com/lorica-labs/volant/compare/v0.1.0-alpha.3...v0.1.0-alpha.4) - 2026-09-09

### Added

- The full host-pattern grammar: wildcards, `!` exclusions and `&` intersections. ([#78](https://github.com/lorica-labs/volant/pull/78))
- The `ssh` connection, with the agent uploaded once and cached on each host. ([#79](https://github.com/lorica-labs/volant/pull/79))
- Agent connections are kept across plays, and `forks` limits how many hosts run at once. ([#82](https://github.com/lorica-labs/volant/pull/82))
- `become` through `sudo`, from keywords and host variables. ([#83](https://github.com/lorica-labs/volant/pull/83))
- A task that reads `hostvars` waits for the other hosts to reach it. ([#85](https://github.com/lorica-labs/volant/pull/85))

### Fixed

- Loop failures, empty loops, and one recap per playbook. ([#86](https://github.com/lorica-labs/volant/pull/86))
- Key order, `omit` in nested structures, and YAML 1.1 integers. ([#87](https://github.com/lorica-labs/volant/pull/87))
- Escalation to any user, a bound on open connections, and exit codes that match Ansible's. ([#90](https://github.com/lorica-labs/volant/pull/90))

## [0.1.0-alpha.3](https://github.com/lorica-labs/volant/releases/tag/v0.1.0-alpha.3) - 2026-09-08

This entry covers the first three pre-releases.

### Added

- `volant playbook` and `volant-playbook`: run playbooks made of `command`, `shell` and `raw` tasks, with ansible-playbook's output, recap and exit codes.
- Static INI inventories with groups, children and group variables; implicit `localhost`.
- The agent binary, running task batches with fail-fast and cancellation.
- Variables from inventories, `group_vars/` and `host_vars/` directories, play `vars` and `vars_files`, task `vars`, `--extra-vars`, `register` and `set_fact`, with Ansible's precedence.
- Jinja2 templating in strict mode, with the common Ansible filters, tests and lookups, checked against ansible-core.
- Task keywords `when`, `loop`, `with_items`, `loop_control`, `register`, `changed_when`, `failed_when`, `timeout`; controller-side `set_fact` and `debug`.
- `--limit`, `ansible.cfg` inventory and timeout, `ansible_version` and `volant_version` variables.
- The `ssh` connection: the agent is uploaded once per host, cached under `remote_tmp` per version, and reused by every later run. Space is checked before the write and the upload is atomic.
- `become` through `sudo`, as a play or task keyword and as `ansible_become*` host variables, with the escalation password read from `-K` or from a variable and never written anywhere. The agent an escalated task runs is cached under the target user's own home, so escalating to an account other than `root` works on a host whose home directories are private.
- `forks`, bounding how many hosts a play works on at once, readable as `ansible_forks`.
- One agent link per host and target user. The host's own link is held for the whole run across plays, with a single reconnect when a link dies between plays; an escalated link lasts as long as the batch that needs it, so `forks` bounds the connections open as well as the hosts working.
- The full host-pattern grammar: wildcards inside names, `!` exclusions, `&` intersections and `[N]`, `[N:M]`, `[N-M]` subscripts.
- New flags `-u`, `--private-key`, `-f`, `-b`, `--become-user`, `--become-method` and `-K`, and the `ansible.cfg` keys `remote_user`, `private_key_file`, `host_key_checking`, `remote_tmp`, `forks` and the `[privilege_escalation]` section.
- Run-level refusals exit with the code the reference gives them: 4 for a playbook that does not parse and for a `vars_files` value of the wrong type, 1 for a playbook file that is not there and for a pattern that leaves no hosts, 2 for a setting that is refused.
- A `--limit` pattern that matches no host warns and narrows the run to what it did match, instead of narrowing it silently.
- A configuration file that is there and cannot be read stops the run instead of falling back to the defaults.

### Changed

- `raw` no longer reports `cmd`, `start`, `end` or `delta`, matching Ansible.
- A host whose connection type is unknown is reported unreachable; the run continues.
- Hosts that fail or become unreachable are left out of the following plays.
- A host reading another host's variables now waits for the others to reach the same task, so `hostvars` holds what the reference would show there.
- `ansible_play_hosts` and its aliases shrink as hosts fail during a play, instead of listing the play's original hosts.
- Each playbook on the command line prints its own recap.
- Mapping keys keep the order the playbook wrote them, in results and in rendered output.

### Fixed

- Interrupting a run now cancels running tasks on every host and kills their whole process group; `SIGTERM` is handled like Ctrl-C.
- A program killed by a signal reports the negative signal number as its return code.
- Banner widths count characters, not bytes.
- A failing loop item now reports its own message and the recap counts it as failed, not as changed; an empty loop skips with `skipped_reason` and an empty `results` list.
- `omit` is removed from lists as well as from dictionaries, at any depth.
- Integers are typed as YAML 1.1 does, so `0o755`, `0x1f` and `1_000` read the way the reference reads them, and the `bool` filter accepts only the spellings Ansible accepts.
- An inventory name that is both a host and a group resolves to the host and warns once per run, as the reference does.
- A group listed as its own descendant no longer drops hosts from `all`, which had made `!web`, `&web` and an empty pattern select the wrong set.
