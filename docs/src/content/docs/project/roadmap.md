---
title: Roadmap
description: What Volant is working toward, release by release, and where to pick up work.
---

Volant is in pre-alpha. The [Volant roadmap board](https://github.com/orgs/lorica-labs/projects/1) holds the plan as GitHub issues, grouped by release. Each issue says what to build, how ansible-core 2.19.12 behaves, how to check the result, and where to start in the code, so anyone can take one. The [changelog](https://github.com/lorica-labs/volant/blob/main/CHANGELOG.md) records what actually shipped.

## Releases

| Milestone | What it brings |
|---|---|
| [v0.1.0-alpha.10](https://github.com/lorica-labs/volant/milestone/2) | The command suite: `volant adhoc`, `volant vault` and `volant inventory` in Rust; `doc`, `config`, `galaxy`, `pull` and `console` through the managed runtime; opt-in `ansible-*` command names, so existing scripts run unchanged without ansible installed |
| [v0.1.0-alpha.11](https://github.com/lorica-labs/volant/milestone/3) | Container connections: docker, podman, kubectl, lxd and incus, chroot, buildah |
| [v0.1.0-alpha.12](https://github.com/lorica-labs/volant/milestone/4) | `volant galaxy` in Rust: collections and roles |
| [v0.1.0-alpha.13](https://github.com/lorica-labs/volant/milestone/5) | `volant pull` and `volant console` in Rust |
| [v0.1.0-alpha.14](https://github.com/lorica-labs/volant/milestone/6) | A Windows controller: `volant.exe` |
| [v0.1.0-alpha.15](https://github.com/lorica-labs/volant/milestone/7) | Windows hosts: winrm, psrp, runas and the PowerShell modules |
| [v0.1.0-alpha.16](https://github.com/lorica-labs/volant/milestone/8) | Network devices (network_cli, netconf, httpapi), paramiko_ssh and the other stdout callbacks |
| [v0.1.0-alpha.17](https://github.com/lorica-labs/volant/milestone/9) | Internal work: shorter executor functions, declarative tables, memory use |
| [v0.1.0-alpha.18](https://github.com/lorica-labs/volant/milestone/10) | A safe switch: record real `ansible-playbook` runs, replay them under Volant without touching hosts, and lock the engine version |

[Parity gaps](https://github.com/lorica-labs/volant/milestone/11) lists the differences from ansible-core found so far. They are not tied to a release, and any of them can be fixed at any time.

## Taking an issue

Comment on the issue before you start, so two people do not do the same work. `size/S` is about a day, `size/M` a few days, and `good first issue` marks a gentle start. [CONTRIBUTING.md](https://github.com/lorica-labs/volant/blob/main/CONTRIBUTING.md) covers the setup and the checks.

Something missing that blocks you? [Open a feature request](https://github.com/lorica-labs/volant/issues/new?template=feature.yml) or start a thread in [Discussions](https://github.com/lorica-labs/volant/discussions).
