<p align="center">
  <img src="https://raw.githubusercontent.com/lorica-labs/volant/main/docs/assets/volant-mark.png" width="88" height="88" alt="">
</p>

# Volant

Fast, drop-in engine for Ansible playbooks.

Volant runs your existing playbooks, roles and inventories unchanged, and runs them faster. It uploads a small static agent once per host and sends tasks in batches instead of one SSH round trip each. Collections, Python modules in a warm interpreter and a `plan` command that shows what would change are what the project is building towards.

**Status: pre-alpha.** A play compiles and runs end to end: `pre_tasks`, roles with their dependencies and argument specs, tasks and `post_tasks`; blocks with `rescue` and `always`; handlers, with `listen`, `meta: flush_handlers` and `--force-handlers`; tags and the four listing commands; `serial` batches; `until` retries; `no_log`; `environment`; `run_once` and `delegate_to`; and `include_tasks`, `include_role` and `include_vars` read while the play runs. Tasks travel over SSH with the agent cached on each host, under Ansible's variable precedence and templating, `become` through `sudo`, `forks` and the full host-pattern grammar. Only the [native modules](docs/src/modules.md) run, and collections, fact gathering and check mode are not there yet; a playbook that needs one of them is refused before the first connection instead of half-run. [Keywords](docs/src/keywords.md) lists what this release executes and what it refuses.

## Installation

Pre-built binaries will be published with every release:

```sh
cargo binstall volant
```

Until then, build from source with a recent stable Rust toolchain:

```sh
cargo install --git https://github.com/lorica-labs/volant volant
```

## Promises

- Your playbooks work as they are. There is no conversion step and no new format.
- Linux targets over SSH first. Windows and network devices come later.
- Volant collects no telemetry and makes no network call other than what your playbook asks for.

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md). Questions go to [Discussions](https://github.com/lorica-labs/volant/discussions), bugs and feature requests to [Issues](https://github.com/lorica-labs/volant/issues).

## License

GPL-3.0-or-later. See [LICENSE](LICENSE).
