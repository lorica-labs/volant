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

### Changed

- `raw` no longer reports `cmd`, `start`, `end` or `delta`, matching Ansible.
- A host whose connection type is unknown is reported unreachable; the run continues.
- Hosts that fail or become unreachable are left out of the following plays.

### Fixed

- Interrupting a run now cancels running tasks on every host and kills their whole process group; `SIGTERM` is handled like Ctrl-C.
- A program killed by a signal reports the negative signal number as its return code.
- Banner widths count characters, not bytes.
