---
title: "Keep the engine GPL and license interface crates MIT OR Apache-2.0"
description: "Decision record 0009."
sidebar:
  label: "0009 · Interface licenses"
  order: 9
---

| Status | Date |
|---|---|
| accepted | 2026-10-24 |

## Context

Other programs want to drive Volant and read what a run did: CI systems, editors, dashboards and AI agents. Today they can only parse the human output, which follows Ansible's text format and is written for people. A documented event stream is planned, with a small crate, `volant-events`, that holds its types and its JSON Schema so a Rust program does not have to define them again.

[Decision 0001](/decisions/0001-gpl-3-0-or-later-license/) put the whole repository under GPL-3.0-or-later. Under that license, any program that links `volant-events` would have to be GPL as well, which would keep most tools from using an interface meant for all of them.

## Decision

The engine stays under GPL-3.0-or-later. That covers `volant`, `volant-agent`, `volant-protocol` and everything else in the repository except the crates below.

A crate whose only purpose is to describe a public interface for other programs is licensed MIT OR Apache-2.0. The first one will be `volant-events`.

Other programs talk to Volant through its command line and its documented event stream. They do not link the engine's crates.

## Consequences

- The engine remains free software. This record is the project's promise that it will not move to a proprietary or source-available license.
- A program under any license can read Volant's events, through `volant-events` or from the JSON Schema alone.
- An interface crate sets `license = "MIT OR Apache-2.0"` in its own `Cargo.toml` instead of inheriting the workspace license, and keeps `LICENSE-MIT` and `LICENSE-APACHE` in its directory. The root `LICENSE` still covers the rest of the repository.
- Contributions to an interface crate are accepted under MIT OR Apache-2.0, signed off under the Developer Certificate of Origin like every other contribution.
- An interface crate is written for its purpose and takes no code from a GPL crate.
- `volant-protocol` stays GPL. Moving it to another license would need the agreement of everyone who contributed to it.
