# Cross-host batching

Volant runs a play the way Ansible's `linear` strategy does: the hosts of a batch meet in front
of every task. Nobody starts task two until everybody has finished task one. The lines a play
prints come in task order because the run itself does.

That guarantee is what lets a playbook depend on something its tasks never mention. A task can
write a file another host reads later, or restart a service another host connects to. None of
that is visible in the text of a task, so the only safe reading is that any task may depend on
the one before it, on every host.

## What the option changes

`batching` lets a host carry on through the tasks between two synchronisation points instead of
waiting at each one. A synchronisation point is then a `run_once` task, or a task whose text
names another host's state through `hostvars`, `ansible_play_hosts` or `ansible_play_batch`.
Everything in between runs host by host.

It is off unless you ask for it, in `ansible.cfg`:

```ini
[volant]
batching = true
```

or in the environment:

```sh
VOLANT_BATCHING=1 volant playbook -i hosts.ini site.yml
```

Ansible skips a section it does not know, so one `ansible.cfg` carrying `[volant]` stays valid
for both engines.

Turning it on changes the order tasks run in, not what they do. The recap is the same either
way. What can differ is a dependency the text does not show: with batching on, a host may reach
task five before another host has run task one, and the file or the service the first host
expects may not be there yet.

## What it costs

Measured on one Linux machine, 200 hosts and 20 tasks over local connections, `-f 50`, a release
build, the median of three runs:

| Playbook | Default | `batching = true` |
|---|---|---|
| No task reads another host | 1.28 s | 0.53 s |
| Every task reads `hostvars` | 1.28 s | 1.25 s |

The second row is the same number twice because those tasks are synchronisation points under
either setting. The first row is the price. A playbook with nothing to synchronise now pays what
the fully synchronised one pays, which on this shape of play is about twice the run.

Privilege escalation pays again. Volant bounds open escalated connections with the same `forks`
permit it bounds working hosts with, and a driver gives both back in front of a wait. By default
every task is a wait, so a play of six tasks under `become` opens six escalated agents where
batching opens one.

The rest of the design is unchanged. The connection and the agent live for the whole run, no
task starts a Python interpreter, and a batch is still one message to one agent. The batch is
now one task wide.

## Why it is not a playbook keyword

Ansible has no such keyword, and a playbook you run with Volant has to stay a playbook you can
run with `ansible-playbook`. A keyword would either break that file for the reference or be
ignored by it, and a setting one of the two engines ignores is worse than no setting at all: the
same playbook would then mean two different things depending on who ran it.

Batching describes the machine you run from rather than the automation you wrote, and that is
where the setting lives.
