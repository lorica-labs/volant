# Use one union blob per warm Python run

- Status: accepted
- Date: 2026-09-20

## Context

The Python path must run ansible-core modules on a managed host without installing ansible-core there. A process per task costs 340 ms. Forking from a Python server that has loaded nothing costs 266 ms, which is not enough to justify the machinery.

Loading `module_utils` from one module's zip and then forking costs 12.8 ms, but it pins Python's `ansible` package to that one zip. Every other module then fails to import. This was measured, and it fails loudly rather than silently.

The entries shared between different module zips are byte-identical: zero conflicts across 146 entries from ten modules. Merging them into one zip is well defined, and the builder treats a conflict as a hard error.

## Decision

Build one union blob per run, not one blob per module. The blob contains every module and its imported `module_utils`, is identified by the BLAKE3 hash of its contents, and is cached on the managed host. The Python server loads it once and forks a child for each task.

The union blob for the five golden modules is exactly 631050 bytes, compared with 2307982 bytes for separate zips. That is a 72.7 percent saving. The preloaded fork path costs 12.8 ms per task, and loading the blob costs 264.5 ms once per server.

## Consequences

- A run gets one consistent `module_utils` set, so modules do not import from incompatible zips.
- The builder must stop on a conflicting shared entry rather than choose one version.
- The managed host caches and verifies the blob before using it.
- The controller needs ansible-core; the managed host does not.

## Measured against a real play

The numbers above come from single modules in a loop. A thirty-five task playbook against one
managed host, run on 2026-09-21, says what they are worth to a real play. It installs packages,
creates directories, writes a configuration line by line, reads files back and holds a service
started, and it escalates, because that is what such a play does.

Three passes each against a host the play had already converged on, so every pass does the same
work, release build. The median, as first measured:

| | wall clock | per task |
|---|---|---|
| Volant | 30.20 s | 863 ms |
| ansible-core, no pipelining | 27.69 s | 791 ms |
| ansible-core, `pipelining = True` | 20.02 s | 572 ms |

That first result was a loss. Volant took 1.51 times as long as the pipelined reference, which is
the figure it has to beat. It has no pipelining setting of its own: it keeps one agent per host
for the whole run, so there is nothing for it to pipeline.

The warm path was not what cost it. Twenty-one tasks with no escalation cost Volant 17 ms each
against the reference's 437 ms, which is the gain this ADR predicted. The same twenty-one under
`become: true` cost 897 ms each against the reference's 464 ms. Counted from the host's own
journal, one run of them opened 44 ssh sessions where an unescalated run opened 4. A host that
reached a barrier gave back its fork permit and shut down its escalated link in the same step,
and a play of Python tasks reaches a barrier at nearly every task, so each escalated task paid for
a new ssh connection, a new `sudo` and a new handshake.

The two did not have to go together. The permit is the run's, and another host is waiting for it.
The escalated link is a process on one host that nothing else is waiting for. Once the link was
kept across the barrier, the same play measured again the same day, same host, same method:

| | wall clock | per task |
|---|---|---|
| Volant, before keeping the link | 30.51 s | 872 ms |
| Volant, after | 7.90 s | 226 ms |
| ansible-core, `pipelining = True` | 19.72 s | 564 ms |

Volant now runs the play in 40 percent of the pipelined reference's time. A pass opens 3 ssh
sessions on the host instead of 58. The before arm reproduces the first table within one percent,
so the gain comes from the fix rather than from a change in the machine.

The per-task average still includes real work. `apt` spends about 755 ms talking to `dpkg`, and
gathering facts is the most expensive task in the play. The warm path removes the overhead around
a module, not the module's own work.

Keeping the link costs something. While the permit carried the link with it, `forks` also bounded
how many escalated connections a run held open. Now a run can hold two links per host, so at three
file descriptors a link, a controller limited to 1024 descriptors reaches its ceiling at about 169
escalating hosts rather than about 330.
