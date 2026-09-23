---
title: Glossary
description: The words this documentation uses in a precise sense.
---

### Agent

A small static program that Volant uploads to each managed host and caches there. It receives tasks from the controller, runs them and streams results back. See [Connections and the agent](/hosts/connections/).

### Batch

The tasks one host runs in a row before it next waits for the others. By default a batch is one task wide. See [Cross-host batching](/hosts/batching/).

### Controller

The machine you run `volant` on, and the `volant` program itself. It reads playbooks, renders templates and drives the agents.

### Flush point

A step where notified handlers run. Every play has three, and `meta: flush_handlers` adds more. See [Handlers](/playbooks/handlers/).

### Link

One open connection from the controller to an agent. A host has its own link, plus one while it escalates and one per delegate.

### Pre-flight

The check that names, before any connection, everything this release does not support yet, and stops the run. See [What is not supported yet](/playbooks/preflight/).

### The reference

ansible-core 2.19.12, the release whose behavior Volant reproduces. A difference from it is a bug unless a page lists it as deliberate. See [decision record 0002](/decisions/0002-ansible-core-2-19-as-reference/).

### Splice point

A step whose followers are only known while the play runs: a handler flush, or a dynamic `include_tasks` or `include_role`. The new steps are inserted there. See [Includes and imports](/playbooks/includes/).

### Step

One entry in the flat, numbered list a play is compiled into. Every host walks the same list. See [How a play runs](/playbooks/how-a-play-runs/).

### Synchronization point

A step where every host of a batch waits for the others. Under the default `linear` behavior, every task is one.

### Union blob

The single archive that carries every Python module a run needs, with the shared code they import. It is sent to each host once. See [The warm Python path](/internals/python/).

### Warm Python server

A Python process the agent keeps running on a host. It loads the shared module code once and forks a child for each task.
