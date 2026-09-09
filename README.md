<p align="center">
  <img src="https://raw.githubusercontent.com/lorica-labs/volant/main/docs/assets/volant-mark.png" width="88" height="88" alt="">
</p>

# Volant

Fast, drop-in engine for Ansible playbooks.

Volant runs your existing playbooks, roles, collections and inventories unchanged, and runs them faster. It uploads a small static agent once per host, sends tasks in batches instead of one SSH round trip each, and runs Python modules from your collections in a warm interpreter. A `plan` command shows what would change before it touches anything.

**Status: pre-alpha.** Playbooks made of the [native modules](docs/src/modules.md) run over SSH today, with the agent uploaded and cached on each host, Ansible's variable precedence and templating, `become` through `sudo`, `forks`, and the full host-pattern grammar. Roles, Python modules from your collections, handlers and fact gathering are not there yet, so most real playbooks will still stop on an unsupported keyword. Follow the releases or watch the repository to know when the first demo build lands.

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
