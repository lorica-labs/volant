# `ansible_version` reports the reference ansible-core release

- Status: accepted
- Date: 2026-09-06

## Context

Many Galaxy roles branch on `ansible_version.full` or `ansible_version.major` to pick a code path. Volant is not Ansible, but it reproduces the behaviour of one ansible-core release (see ADR 0002). Leaving `ansible_version` undefined would fail those roles on an undefined variable; inventing a Volant-specific value would send them down untested paths.

## Decision

`ansible_version` holds the reference release Volant reproduces, in the dictionary shape roles expect (`full`, `major`, `minor`, `revision`, `string`). A separate `volant_version` variable holds Volant's own version.

## Consequences

- Roles written for the reference release behave as they do under it.
- When the reference moves (ADR 0002 process), `ansible_version` moves with it, in the same change.
- A playbook that needs to tell Volant apart from Ansible tests `volant_version is defined`.
