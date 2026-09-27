---
title: Facts
description: How Volant gathers facts, the two names each fact is readable under, and what is not supported yet.
---

A play gathers facts unless it sets `gather_facts: false`. Volant either runs ansible-core's own `setup` module on the [warm Python path](/internals/python/), or lets the agent collect the facts natively when the playbook provably reads nothing else, see [Native facts](#native-facts).

```yaml
- hosts: web
  tasks:
    - debug:
        msg: "{{ ansible_hostname }} runs {{ ansible_facts.distribution }}"
```

Because `setup` can always fall back to its Python module, the controller needs ansible-core and the host needs a Python 3 interpreter. A controller without ansible-core stops a play that gathers facts before the first connection, instead of carrying on without them. Set `gather_facts: false` on plays that do not need facts.

## Two names for each fact

Each fact is readable in two places:

- flat, with an `ansible_` prefix: `ansible_hostname`
- inside `ansible_facts`, spelled the way the module returned it: `ansible_facts.hostname`

They are separate variables. A later `set_fact` of `ansible_hostname` shadows the flat one and leaves `ansible_facts.hostname` alone. Ansible behaves the same way.

Gathered facts rank above inventory variables and below the play's own variables, see [Precedence](/variables/precedence/).

## Native facts

On Debian and Ubuntu, the agent can collect facts itself, in the reference's own words: the 17 collectors ansible-core runs for `gather_subset: min`, the processor and memory facts of its `hardware` collector, and the default routes and address lists of its `network` collector. A fact the native collector does not produce, such as mounts, devices or per-interface details, is left absent rather than guessed.

An absent fact only matters if something reads it. So before the first connection, Volant reads every play, role, template and variable definition of the run and lists the facts they can read. It uses the native collector only if all of them are facts it produces. A read by a computed name, such as `ansible_facts[item]`, `vars['ansible_' ~ name]`, `lookup('vars', ...)` or a whole `hostvars[host]` passed to a filter, could reach any fact, so it keeps ansible-core's `setup`.

The native collector also hands the task back to the Python module on a host it cannot reproduce: another distribution, SELinux enabled, local facts in `/etc/ansible/facts.d`, a host name only DNS knows.

`--facts python` always runs ansible-core's module, and `--facts native` always asks the agent. `--profile` prints which one the run chose and why. See [Command line](/reference/cli/#run-and-output).

## Facts are data

Facts are the host's own words, so they arrive untrusted: a template inside a fact stays text and is never rendered. See [Trusted and untrusted values](/variables/trust/).

## Not there yet

- `gather_subset` and `gather_timeout` are not supported yet. Remove them and the play gathers the full set of facts.
