# Action plugins run as a controller-driven sequence of sub-tasks

- Status: accepted
- Date: 2026-09-23

## Context

`copy`, `package`, `service`, `template` and `unarchive` are not, in the reference, single
modules a host runs on a payload the controller sends. `plugins/action/copy.py` runs `stat` on
the host first, and decides from its answer whether to run `file` or to stage the source and run
`copy`. `plugins/action/package.py` and `plugins/action/service.py` read a fact (the host's
package manager, its init system) and only then know which module to run at all; when the fact
is not already gathered, they run a filtered `setup` themselves to get it. `template` reads and
renders a file on the controller before anything reaches the host. In each case, what the
playbook names is not what a module payload alone can carry out: the decision the reference makes
between one host round trip and the next is part of the task.

Two shapes were open for reproducing this. One module payload could be rewritten to make that
decision itself, on the host, in the process that already runs `stat` or reads the fact: moving
`copy.py`'s or `package.py`'s own logic into the agent's Python side. Or the action plugin's own
role could run as a second module, uploaded and executed on the host in place of the controller
process that the reference uses. Both would keep the controller out of a loop the reference itself
keeps it in: the reference decides between sub-tasks on the controller, in Python, reading each
sub-task's result before choosing the next one.

## Decision

Each of the five runs as a small state machine on the controller: asked for the next sub-task,
handed the result of the last one, until it has the task's own result. Every sub-task is an
ordinary module of the run's union, sent alone over the link the task already has and read back
through the same path any other module result takes. Nothing about a sub-task's dispatch,
argument or file handling lives on the host; the host only ever runs modules it already knows how
to run.

`copy` is the plugin that makes the two-call shape explicit: `stat` first, then either `file` or a
staged `copy`, exactly as `plugins/action/copy.py` does it. `package` and `service` are the ones
that make the fact dependency explicit: the module that runs is not fixed until the host's package
manager or init system is known, gathered through a filtered `setup` when it is not already a
fact. `template` and `unarchive` both read a file on the controller before a sub-task is sent,
`template` always and `unarchive` unless `remote_src` says the file is already on the host.

## Consequences

- A file a sub-task needs travels to the host once, staged in a directory private to the
  connection that sent it, and is consumed by that one task: read by the module, then removed,
  whatever the module did with it. See [Action plugins](../src/actions.md#what-stays-on-the-host).
- `package` and `service` carry every backend module they might dispatch to in the run's union,
  not only the one the host turns out to need, because nothing is known about the host until the
  facts are in and nothing is built afterwards. Measured against ansible-core 2.19.12's own
  payload builder, that is about 170 KB more sent once per link than `apt` and `systemd_service`
  alone would cost.
- A `src` whose render read a managed host is refused before it is looked up, for `copy`,
  `template` and `unarchive` alike, rather than sent the way the reference sends it. This is a
  security decision, not a gap: measured against the reference, a `copy` (or `template`, or
  `unarchive`) whose `src` is a registered value sends whatever controller file that value names
  to the host, so a host that controls a command's output or a fact controls which file of the
  operator's it receives. See
  [Action plugins](../src/actions.md#a-source-file-a-managed-host-named).

## Results

Measured on 2026-09-23 with four roles from Ansible Galaxy, run as published:
`geerlingguy.security` 3.0.2, `geerlingguy.nginx` 3.3.1, `geerlingguy.git` 3.0.1 and
`geerlingguy.pip` 3.1.2, in one play with `become: true` and facts gathered (`just proof-roles`).
The managed hosts are two machines running Ubuntu 24.04. The reference is ansible-core 2.19.12.

`just proof-roles-reset` removed the packages and files the roles install, so both engines
started from the same state. Their first passes ended on the same recap,
`ok=30 changed=8 skipped=28`, and their second passes on `ok=28 changed=0 skipped=28`.

Each timing is the median of three passes against hosts the roles had already converged, with a
release build of Volant. A pass shows 56 tasks per host.

| | one host | two hosts |
|---|---|---|
| ansible-core, no pipelining | 33.50 s | 34.52 s |
| ansible-core, `pipelining = True` | 25.70 s | 26.34 s |
| Volant | 12.49 s | 12.64 s |

Volant takes 49 percent of the pipelined reference's time on one host and 48 percent on two. The
controller builds the union once per run. On the first pass against two hosts it landed in each
host's cache, and the second pass left the cached file untouched.
