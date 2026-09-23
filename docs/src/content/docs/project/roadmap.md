---
title: Roadmap
description: What Volant is working toward, in the order it is likely to land.
---

Volant is in pre-alpha. This page lists what comes next, roughly in order. It describes intent, not dates, and the [changelog](https://github.com/lorica-labs/volant/blob/main/CHANGELOG.md) records what actually shipped.

## Next: the modules real roles use

- The remaining modules backed by an action plugin: `fetch`, `reboot`, `uri`, `script` and their relatives. `copy`, `package`, `service`, `template` and `unarchive` already run.
- The Jinja2 filters, tests and lookups that popular Galaxy roles use.
- Modules from collections, and `dnf`.
- `with_first_found` and `with_fileglob`.

The goal is to run well-known Galaxy roles end to end and produce the same recap as `ansible-playbook`.

## Then: speed

- Native versions of the most common modules in the agent, falling back to the Python path when an argument needs it.
- Native fact gathering.
- A faster warm Python server.
- Fewer synchronization points, found by analyzing the playbook.
- Compression and a leaner wire format.

## Then: ready for daily use

- Vault.
- `--check` and `--diff`.
- YAML inventories.
- A `volant check` command that tells you whether a project will run, and what blocks it.
- A Homebrew formula, and `cargo binstall` with the agents included.
- A nightly run over a corpus of public roles, compared with ansible-core.
- Video walkthroughs of the main workflows.

## Later

- Controller-side plugins from collections.
- Dynamic inventories.
- Windows hosts and network devices.

Something missing that blocks you? [Open a feature request](https://github.com/lorica-labs/volant/issues/new?template=feature.yml) or start a thread in [Discussions](https://github.com/lorica-labs/volant/discussions).
