---
title: From 12.66 to 0.529 seconds on four Galaxy roles
description: What a per-step profile showed on 2026-09-24, what changed in the three days after it, and the options that were measured and dropped.
date: 2026-09-27
draft: true
---

On 2026-09-24, Volant applied four Galaxy roles (`geerlingguy.security`, `nginx`, `git` and `pip`) to two Ubuntu 24.04 hosts that had already converged in 12.66 seconds. ansible-core 2.19.12 with pipelining took 26.23 seconds. Three days later, on the same machines, Volant took 0.529 seconds and ansible-core 26.435. Both engines ended every pass on the same recap. This post goes through what changed.

## Where the time went

A per-step profile of the 12.66-second run showed, per host:

- 5.5 seconds in eight `apt` tasks with nothing to install, because every call imported and ran the Python module;
- 3.8 seconds rebuilding the set of Python modules sent to the host, although nothing in ansible-core had changed since the last run;
- 1.1 seconds gathering facts, 0.7 in three `systemd` tasks and 0.7 in `file`, `lineinfile` and `stat`;
- 0.6 seconds opening connections, because two steps each paid their own SSH handshake.

The frames on the wire took 30 milliseconds in total, so the protocol was not the problem.

## What changed

Common modules got a version in Rust, inside the agent on the host: `setup`, `stat`, `file`, `apt`, `systemd`, `lineinfile`, `copy`, `package_facts`, `service_facts`, `user` and `group`. A native module receives the arguments the Python module would have received. When they fall within what it implements, it answers with the result the Python module would give. Otherwise it hands the task back to the Python module, before anything on the host has changed. A native module is turned on only when its answers match ansible-core key for key in every case the test suite records.

The set of Python modules is cached on the controller and rebuilt only when something it was built from changes. Checking it costs about a millisecond.

Each host gets one OpenSSH connection, shared by every step of the run through `ControlMaster`, unless your SSH configuration already says something about connection sharing.

| Step, per host | 2026-09-24 | 2026-09-27 |
|---|---|---|
| Building the Python module set | 3,799 ms | 15.3 ms, from the cache |
| `apt`, 8 tasks | 5,495 ms | 38.4 ms, native |
| Gathering facts | 1,094 ms | 47.6 ms, native |
| `systemd`, 3 tasks | 730 ms | 52.4 ms, native |
| `file`, `lineinfile` and `stat` | 725 ms | 52.8 ms, native |
| Opening the connections | 555 ms | 172.6 ms |

## What was measured and dropped

A Rust SSH client saved 8 milliseconds per new channel, and would have added 115 crates to the agent and lost most of what `~/.ssh/config` can say. Binary frame formats saved about a millisecond on a whole run. Neither was kept.

The figures, the configuration and the commands that reproduce them are on the [performance page](/performance/). [Decision record 0008](/docs/decisions/0008-native-modules-and-speed/) has the full reasoning.
