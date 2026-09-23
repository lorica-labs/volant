# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- Run `copy` as an action plugin: `stat` the destination, then `file` or a staged `copy`, sending nothing when the file already matches.
- Run `package` as an action plugin: pick the module from the host's package manager, gathered with a filtered `setup` when it is not already a fact.
- Run `service` as an action plugin: pick the module from the host's init system, the same way `package` picks its own.
- Run `template` as an action plugin: render the file on the controller once, then hand it to the same path `copy` uses.
- Run `unarchive` as an action plugin: extract a controller archive or one already on the host, skipping the task when `creates:` already exists.
- Add the Jinja filters, tests, methods and lookups the template engine was missing: `b64decode`, `b64encode`, `comment`, `difference`, `intersect`, `union`, `flatten`, `from_yaml`, `to_yaml`, `to_nice_yaml`, `to_uuid`, `type_debug`, `quote` and `regex_escape`; the `changed`, `failed`, `succeeded`, `skipped` and `version` tests; a handful of Python methods on strings and mappings, through `minijinja-contrib`; and the `first_found` and `template` lookups.

### Changed

- A `copy`, `template` or `unarchive` task whose `src` was named by a managed host is now refused before the file is looked up, rather than sent the way the reference sends it: a host that controls a command's output or a fact could otherwise have any file the operator can read copied to it.

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

- *(modules)* declare which arguments each native module honours ([#145](https://github.com/lorica-labs/volant/pull/145))
- *(executor)* put a barrier in front of every linear task ([#140](https://github.com/lorica-labs/volant/pull/140))

### Fixed

- *(preflight)* check what a dynamic include splices in ([#155](https://github.com/lorica-labs/volant/pull/155))
- *(command)* hold the deadline over the stdin write ([#154](https://github.com/lorica-labs/volant/pull/154))
- *(transport)* refuse a remote_tmp whose tilde part is not a user name ([#146](https://github.com/lorica-labs/volant/pull/146))
- *(command)* keep the deadline over the child and its streams ([#144](https://github.com/lorica-labs/volant/pull/144))
- *(executor)* show every event a task produces through the renderer ([#143](https://github.com/lorica-labs/volant/pull/143))
- *(command)* resolve creates and removes where the command runs ([#142](https://github.com/lorica-labs/volant/pull/142))
- *(transport)* resolve the connection from effective host variables ([#141](https://github.com/lorica-labs/volant/pull/141))
- *(template)* treat module results as data, never as templates ([#139](https://github.com/lorica-labs/volant/pull/139))

### Other

- follow the branch ref, not just .git/HEAD ([#153](https://github.com/lorica-labs/volant/pull/153))
- send the ssh quickstart to the host it names ([#152](https://github.com/lorica-labs/volant/pull/152))
- say what this release expands and when links close ([#150](https://github.com/lorica-labs/volant/pull/150))
- say what this release runs and what it only partly answers ([#148](https://github.com/lorica-labs/volant/pull/148))
- ship the Linux agents inside every controller archive ([#147](https://github.com/lorica-labs/volant/pull/147))
- make the coverage floor fail the recipe again ([#137](https://github.com/lorica-labs/volant/pull/137))
- *(vars)* share the inventory-wide values across hosts ([#134](https://github.com/lorica-labs/volant/pull/134))
- *(executor)* say why in one line and move the rest out ([#133](https://github.com/lorica-labs/volant/pull/133))
- *(executor)* split the runner into modules ([#132](https://github.com/lorica-labs/volant/pull/132))
- *(executor)* give the host driver its own context ([#131](https://github.com/lorica-labs/volant/pull/131))
- *(executor)* hold the coordinator state in one place ([#130](https://github.com/lorica-labs/volant/pull/130))
- *(executor)* drop a guard nothing depends on ([#129](https://github.com/lorica-labs/volant/pull/129))
- add a mutation recipe and the checks it found missing ([#128](https://github.com/lorica-labs/volant/pull/128))
- declare workspace lints and name each allowance ([#123](https://github.com/lorica-labs/volant/pull/123))

## [0.1.0-alpha.5](https://github.com/lorica-labs/volant/compare/v0.1.0-alpha.4...v0.1.0-alpha.5) - 2026-09-16

### Added

- *(controller)* run_once and delegate_to on the delegate's own link ([#115](https://github.com/lorica-labs/volant/pull/115))
- *(controller)* include tasks, roles and vars at run time ([#109](https://github.com/lorica-labs/volant/pull/109))
- *(controller)* run plays in serial batches and stop when one fails ([#108](https://github.com/lorica-labs/volant/pull/108))
- *(controller)* retry with until, censor no_log, pass environment ([#107](https://github.com/lorica-labs/volant/pull/107))
- *(controller)* run notified handlers at every flush point ([#105](https://github.com/lorica-labs/volant/pull/105))
- *(controller)* run rescue and always, set ansible_failed_* ([#104](https://github.com/lorica-labs/volant/pull/104))
- *(controller)* select tasks by tag and list them like the reference ([#103](https://github.com/lorica-labs/volant/pull/103))
- *(controller)* load roles from the standard paths and import tasks ([#102](https://github.com/lorica-labs/volant/pull/102))
- *(controller)* compile plays into flat steps with block spans ([#101](https://github.com/lorica-labs/volant/pull/101))
- *(controller)* load the whole grammar, refuse before connecting ([#98](https://github.com/lorica-labs/volant/pull/98))

### Fixed

- *(controller)* elect run_once in the coordinator and free the permit ([#118](https://github.com/lorica-labs/volant/pull/118))

### Other

- describe play compilation, roles, blocks, handlers and tags ([#117](https://github.com/lorica-labs/volant/pull/117))
- *(controller)* share hostvars and keep the fork permit ([#116](https://github.com/lorica-labs/volant/pull/116))

## [0.1.0-alpha.4](https://github.com/lorica-labs/volant/compare/v0.1.0-alpha.3...v0.1.0-alpha.4) - 2026-09-09

### Added

- *(controller)* wait for the other hosts before reading hostvars ([#85](https://github.com/lorica-labs/volant/pull/85))
- *(controller)* escalate with sudo via become keywords and variables ([#83](https://github.com/lorica-labs/volant/pull/83))
- *(controller)* keep agent connections across plays and honour forks ([#82](https://github.com/lorica-labs/volant/pull/82))
- *(controller)* reach hosts over ssh and cache the agent remotely ([#79](https://github.com/lorica-labs/volant/pull/79))
- *(controller)* add wildcards, negation and intersection to patterns ([#78](https://github.com/lorica-labs/volant/pull/78))

### Fixed

- *(controller)* escalate to any user, bound links, match exit codes ([#90](https://github.com/lorica-labs/volant/pull/90))
- *(controller)* key order, nested omit and yaml 1.1 integers ([#87](https://github.com/lorica-labs/volant/pull/87))
- *(controller)* loop failures, empty loops, per-playbook recaps ([#86](https://github.com/lorica-labs/volant/pull/86))

### Other

- describe connections, the agent cache and privilege escalation ([#89](https://github.com/lorica-labs/volant/pull/89))
- run the ssh end-to-end tests against a local sshd ([#81](https://github.com/lorica-labs/volant/pull/81))
- let release-plz own the repository changelog ([#75](https://github.com/lorica-labs/volant/pull/75))

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
