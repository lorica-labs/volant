---
title: "Use ansible-core 2.19 as the compatibility reference"
description: "Decision record 0002."
sidebar:
  label: "0002 · Compatibility reference"
  order: 2
---

| Status | Date |
|---|---|
| accepted | 2026-09-05 |

## Context

Volant is built to run existing playbooks unchanged. "Unchanged" needs a definition: ansible-core changes behavior between versions, and 2.19 rewrote templating (data tagging) with some intentional breaks.

## Decision

Volant reproduces the behavior of ansible-core 2.19. Differences from 2.19 are bugs. Golden tests and the compatibility harness compare against that version. A nightly job against ansible-core's development branch, to see changes coming before they land in a release, is planned but has not been built: nothing watches that branch today.

## Consequences

- Playbooks that only worked on older cores because of behavior removed in 2.19 are out of scope.
- Moving the reference to a newer core is a new decision, recorded here, with a major or minor version bump depending on what changes.
