---
title: Decision records
description: The decisions that shape what Volant users see, and why they were taken.
sidebar:
  label: About decision records
  order: 0
---

A decision record explains one choice that affects Volant's users: what the situation was, what was decided, and what follows from it. The records use the [MADR](https://adr.github.io/madr/) format and are numbered in the order they were taken.

A record is not rewritten once it is accepted. When a later decision changes it, the status line says so and links to the new record. Measurements taken after the decision are added as dated sections at the end.

| Record | Decision |
|---|---|
| [0001](/decisions/0001-gpl-3-0-or-later-license/) | License the whole repository under GPL-3.0-or-later. |
| [0002](/decisions/0002-ansible-core-2-19-as-reference/) | Reproduce ansible-core 2.19, and treat any difference as a bug. |
| [0003](/decisions/0003-ansible-version-reports-the-reference-release/) | `ansible_version` reports the reference release; `volant_version` reports Volant's. |
| [0004](/decisions/0004-become-runs-a-second-agent-under-sudo/) | `become` starts a second agent under `sudo`. |
| [0005](/decisions/0005-fork-permits-span-host-local-batches/) | A host keeps its fork permit across host-local steps, behind the `batching` option. |
| [0006](/decisions/0006-warm-python-path/) | Send one union blob of Python modules per run, and fork them from a warm server. |
| [0007](/decisions/0007-action-plugins/) | Run action plugins as a sequence of sub-tasks driven by the controller. |
