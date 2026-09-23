---
title: Facts
description: How Volant gathers facts, the two names each fact is readable under, and what is not supported yet.
---

A play gathers facts unless it sets `gather_facts: false`. Volant runs ansible-core's own `setup` module for that, on the [warm Python path](/internals/python/).

```yaml
- hosts: web
  tasks:
    - debug:
        msg: "{{ ansible_hostname }} runs {{ ansible_facts.distribution }}"
```

Because `setup` is a Python module, the controller needs ansible-core and the host needs a Python 3 interpreter. A controller without ansible-core stops a play that gathers facts before the first connection, instead of carrying on without them. Set `gather_facts: false` on plays that do not need facts.

## Two names for each fact

Each fact is readable in two places:

- flat, with an `ansible_` prefix: `ansible_hostname`
- inside `ansible_facts`, spelled the way the module returned it: `ansible_facts.hostname`

They are separate variables. A later `set_fact` of `ansible_hostname` shadows the flat one and leaves `ansible_facts.hostname` alone. Ansible behaves the same way.

Gathered facts rank above inventory variables and below the play's own variables, see [Precedence](/variables/precedence/).

## Facts are data

Facts are the host's own words, so they arrive untrusted: a template inside a fact stays text and is never rendered. See [Trusted and untrusted values](/variables/trust/).

## Not there yet

- `gather_subset` and `gather_timeout` are not supported yet. Remove them and the play gathers the full set of facts.
