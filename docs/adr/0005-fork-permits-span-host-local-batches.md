# Fork permits span host-local batches

- Status: accepted, and conditional on an option since 2026-09-20 (see Status below)
- Date: 2026-09-16

## Context

A host driver takes one of `forks` permits before it opens a connection and gives it back when the batch it collected has answered. ADR 0004 left live agent connections unbounded; tying the escalated links to the permit is what bounds them, because `forks` then limits the connections open as well as the hosts working. Twenty hosts escalating to root under `ulimit -n 128` and `-f 5` used to report `Too many open files` for a host that was perfectly reachable.

Handing the links back at every batch is not free, and a batch ends more often than the word suggests. Any of these closes one: a task carrying `register`, `loop`, `changed_when` or `failed_when`; a task that reads another host's state, which is a boundary in front of itself; and a task escalating to a different user. Registering a result is ordinary, so a play of six escalated tasks each with a `register` paid six escalations, each one a `sudo` probe and a `sudo` that starts the agent. Measured on the development machine with a `sudo` that records what it is asked to run: twelve invocations for those six tasks.

Nothing about a `register` needs another host, so the permit was going back in front of a wait that was not there.

## Decision

A driver keeps its fork permit and its escalated links from one batch to the next when it is about to carry straight on with a step of its own. It gives both back in front of every wait on the other hosts, and at the end of the play, where holding them would only keep descriptors busy. A driver that leaves the run through a failure gives them back too.

Four steps count as a wait: one the keyword table declares a synchronisation point, one whose text names another host's state, a splice point where the step list grows, and a step whose successor sits behind one. So does a step held back until its own result decides where the host goes.

Escalated links are closed off-task rather than awaited, as before: `shutdown` grants its agent up to two seconds, and a driver paying that between batches would serialise what releasing the permit just freed.

This decision adds no connection multiplexing. Every link remains its own `ssh` process.

## Consequences

- Live links are bounded by one per host, for the connection that outlives the play, plus `forks` times the number of distinct target users a driver escalates to while it holds one permit. The earlier bound counted one escalated link per driver because each batch closed its own; several consecutive batches under one permit can now name different users.
- Measured, twenty hosts at `-f 5` escalating to root: the controller peaks at 84 open descriptors where it peaked at 69 before, against a limit of 128. The run exits 0 with no unreachable host, which is the same proof the earlier bound was accepted on.
- A play of six escalated tasks, each closing its batch with a `register`, starts one escalated agent instead of six.
- Hosts take their turn at a wait rather than at every batch. With `forks` under the number of live hosts and a play whose steps are all host-local, a host now runs its remaining steps before the next one starts, where the two used to alternate. The work is the same and so is the recap; what changes is the order lines arrive in, which was already unordered between two hosts with nothing to synchronise them.
- A play that never escalates is unaffected beyond holding its permit a little longer, and a play whose every task reads across hosts gives the permit back at every step, as before.

Supersedes the "live agent connections per host are not bounded" consequence of ADR 0004.

## Status

Since 2026-09-20, a driver carries its permit past a host-local step only when the operator asks
for it. `[volant] batching = true` in `ansible.cfg`, or `VOLANT_BATCHING=1`, turns this decision
on; without it every step is a wait, as `linear` says it is.

The reason is the fourth consequence above. Which steps count as a wait is read from the text of
a task, and a dependency that runs through a file, a database or a service leaves no text to
read. Ansible's `linear` promises that the hosts meet in front of every task, and a playbook
written against that promise is allowed to rely on it. Deciding otherwise from the text alone
was the wrong default for an engine whose first promise is that playbooks run unchanged.

Everything the decision describes is kept and still runs under the option. The measured cost of
the two settings, and what the default keeps either way, are in [Cross-host
batching](../src/batching.md).
