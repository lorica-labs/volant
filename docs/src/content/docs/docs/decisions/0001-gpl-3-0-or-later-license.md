---
title: "Use GPL-3.0-or-later for the whole repository"
description: "Decision record 0001."
sidebar:
  label: "0001 · License"
  order: 1
---

| Status | Date |
|---|---|
| accepted; the scope is narrowed by [ADR 0009](/docs/decisions/0009-interface-crates-mit-or-apache/), which licenses crates that describe a public interface MIT OR Apache-2.0 | 2026-09-05 |

## Context

Volant is built to execute unmodified Ansible modules. To do so, the controller will call ansible-core's own payload builder through an embedded Python interpreter, and the agent will ship ansible-core's `module_utils` to managed hosts. ansible-core is licensed under GPL-3.0. Importing a GPL library at runtime and redistributing parts of it makes a permissive license for Volant legally fragile.

This release is not there yet. It embeds no Python interpreter, carries no ansible-core code, and runs only the native modules. The license is picked for the architecture the project is heading for rather than for the one it has, because a repository whose contributors have all signed off under one license is hard to move to another.

## Decision

The whole repository is licensed GPL-3.0-or-later, the license of Ansible and of most collections.

## Consequences

- Contributors sign off their commits under the Developer Certificate of Origin.
- Adoption is not affected: Ansible itself is a GPL command-line tool.
- When the payload builder is rewritten in Rust and the agent no longer contains ansible-core code, the agent crate may be relicensed under Apache-2.0. That will be a new decision.

## Update, 2026-09-22

The context above describes the project when this decision was taken. Since [ADR 0006](/docs/decisions/0006-warm-python-path/), Volant ships ansible-core's `module_utils` to managed hosts, and the controller calls ansible-core through a helper process rather than an embedded interpreter. The reasoning for the license is unchanged.

## Update, 2026-10-24

[Decision 0009](/docs/decisions/0009-interface-crates-mit-or-apache/) keeps the engine under GPL-3.0-or-later and licenses the crates that only describe a public interface MIT OR Apache-2.0. It also keeps `volant-agent` and `volant-protocol` under GPL-3.0-or-later, so the possible relicensing of the agent crate named in the consequences above is not planned.
