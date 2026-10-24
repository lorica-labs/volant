---
title: Volant 0.1.0-alpha.10
description: su over a local connection, a checksum for packed runtimes, and twenty fixes that bring errors and output closer to ansible-core.
date: 2026-10-23
draft: true
---

Volant 0.1.0-alpha.10 is out. Install it with the usual command, or update an existing install the same way:

```sh
curl -fsSL https://volant.sh/install.sh | sh
```

## What is new

- `become_method: su` now works over a `local` connection, not only over SSH ([#755](https://github.com/lorica-labs/volant/pull/755)).
- `volant runtime pack` prints the SHA-256 of the archive it writes, and `volant runtime install --from FILE --sha256 HASH` refuses the file before unpacking when its hash differs ([#769](https://github.com/lorica-labs/volant/pull/769)). Moving a runtime to a machine without a network now has a checksum at both ends.
- `force_color`, `nocolor` and `deprecation_warnings` are read from `ansible.cfg` and their environment variables ([#752](https://github.com/lorica-labs/volant/pull/752)), and so are `usetty` and `ANSIBLE_SSH_USETTY` ([#753](https://github.com/lorica-labs/volant/pull/753)).

## What is fixed

Most of the fixes make a failure look the way it does under ansible-core, with the same message and the same exit code:

- A vaulted value that no secret opens fails the task that reads it ([#774](https://github.com/lorica-labs/volant/pull/774)).
- An unknown `meta` action, an `action:` that names no module and a role whose tasks do not load now fail when the task is reached, after its banner ([#761](https://github.com/lorica-labs/volant/pull/761)).
- An undefined variable, a lookup that fails and a filter that raises are worded and chained as ansible-core does, also inside `when` and in template files ([#754](https://github.com/lorica-labs/volant/pull/754), [#768](https://github.com/lorica-labs/volant/pull/768), [#770](https://github.com/lorica-labs/volant/pull/770)).
- Skipped, `no_log` and `add_host` results print as ansible-core prints them ([#757](https://github.com/lorica-labs/volant/pull/757)).

Two fixes change how action plugins from collections run. Their jobs now run side by side, and a command such a plugin marks as not to be escalated runs as the connection user ([#776](https://github.com/lorica-labs/volant/pull/776)). Lookups that run through Python receive the play's variables ([#775](https://github.com/lorica-labs/volant/pull/775)).

The [changelog](/docs/changelog/) lists every change of the release.
