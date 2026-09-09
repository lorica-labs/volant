# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- `volant playbook` and `volant-playbook`: run playbooks made of `command`, `shell` and `raw` tasks on hosts with `ansible_connection=local`, with ansible-playbook's output, recap and exit codes.
- Static INI inventories with groups, children and group variables; implicit `localhost`.
- The agent binary, uploaded to hosts in later releases, running task batches with fail-fast and cancellation.
- Variables from inventories, `group_vars/` and `host_vars/` directories, play `vars` and `vars_files`, task `vars`, `--extra-vars`, `register` and `set_fact`, with Ansible's precedence.
- Jinja2 templating in strict mode, with the common Ansible filters, tests and lookups, checked against ansible-core.
- Task keywords `when`, `loop`, `with_items`, `loop_control`, `register`, `changed_when`, `failed_when`, `timeout`; controller-side `set_fact` and `debug`.
- `--limit`, `ansible.cfg` inventory and timeout, `ansible_version` and `volant_version` variables.
- The `ssh` connection: the agent is uploaded once per host, cached under `remote_tmp` per version, and reused by every later run. Space is checked before the write and the upload is atomic.
- `become` through `sudo`, as a play or task keyword and as `ansible_become*` host variables, with the escalation password read from `-K` or from a variable and never written anywhere.
- `forks`, bounding how many hosts a play works on at once, readable as `ansible_forks`.
- One agent link per host and target user, held for the whole run across plays, with a single reconnect when a link dies between plays.
- The full host-pattern grammar: wildcards inside names, `!` exclusions, `&` intersections and `[N]`, `[N:M]`, `[N-M]` subscripts.
- New flags `-u`, `--private-key`, `-f`, `-b`, `--become-user`, `--become-method` and `-K`, and the `ansible.cfg` keys `remote_user`, `private_key_file`, `host_key_checking`, `remote_tmp`, `forks` and the `[privilege_escalation]` section.

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
