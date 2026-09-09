# `become` runs a second agent under `sudo`

- Status: accepted
- Date: 2026-09-09

## Context

Ansible escalates privileges per task, wrapping each module invocation in `sudo`. Volant runs tasks through a long-lived agent per host, so wrapping each task would mean either an agent that calls `sudo` for every module, or an agent that is itself privileged for the whole play.

## Decision

The controller opens one agent connection per (host, target user). A task with `become` runs through the agent started as `sudo -H <form> -u <user> -- volant-agent`, where `<form>` is `-n` or `-k -S -p ''` as the probe described below decides; tasks without it keep the unprivileged agent. Only `sudo` is supported; other methods are refused by name. The agent itself does not know about escalation.

## Consequences

- A change of target user between two consecutive tasks ends the batch; the common case, a whole play under `become`, keeps one batch.
- The form of the `sudo` invocation is settled by a probe, not by whether a password was supplied. A `sudo` that needs no authentication does not read its standard input, so on a host with a cached authentication or a `NOPASSWD` rule, a password written there is left on the pipe and the *agent* reads it as the header of its first frame. The controller therefore asks `sudo -n` first and only falls back to `sudo -k -S -p ''` once `sudo` has itself refused for want of authentication. Guessing the form from the password alone hangs the link, and only on the second escalated connection inside the timestamp window, which is why the probe is not optional.
- The password, when one is given, is written once to `sudo`'s standard input and never appears in a command line, a log or a result. The two types that carry it have hand-written `Debug` implementations that print `<redacted>`, so a `-vvv` dump cannot leak it, and `sudo`'s own captured standard error is scrubbed of it before it can reach a message.
- A failed escalation is a failed task, not an unreachable host: the connection worked.
- Live agent connections per host are not bounded. `forks` bounds how many hosts run at once, not how many links one host holds, and the map keeps one link per distinct target user until the recap, each of them a separate `ssh` process. An escalated link also costs one extra `ssh` connection for the probe, two when a password is wanted. This decision adds no connection multiplexing: every link is its own `ssh` process, and a wide inventory pays for each of them separately.
- `su`, `doas`, `pbrun` and the rest can be added as other ways to start the agent, without a protocol change.
