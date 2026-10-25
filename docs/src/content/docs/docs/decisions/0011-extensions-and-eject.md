---
title: "Extend Ansible only through what can be ejected back to it"
description: "Decision record 0011."
sidebar:
  label: "0011 · Extensions and eject"
  order: 11
---

| Status | Date |
|---|---|
| accepted | 2026-10-25 |

## Context

Volant runs Ansible playbooks as ansible-core 2.19 does ([decision 0002](/docs/decisions/0002-ansible-core-2-19-as-reference/)). Once that holds, it can offer what Ansible lacks. The risk is a project that starts to depend on such a feature and can no longer go back to ansible-core. A team adopts Volant more easily when it knows it can leave at any time.

Two things make leaving possible. YAML stays the source: nothing replaces it. And every extension has a rule that rewrites it into plain Ansible with the same behaviour, applied by a command.

## Decision

### Two kinds of extension

Content that Ansible can load, such as filters, lookups and modules, goes in the `volant.compat` collection. It works under ansible-core as it is, so it never needs ejecting.

Keywords and syntax that only Volant understands are spelled `volant.<keyword>`, as a key of the play, block or task they apply to:

```yaml
- name: one account per user
  volant.loop: "{{ users }}"
  block:
    - ansible.builtin.user:
        name: "{{ item }}"
    - ansible.posix.authorized_key:
        user: "{{ item }}"
        key: "{{ lookup('file', 'keys/' ~ item ~ '.pub') }}"
```

A key rather than a `volant:` mapping, because each extension is then one key that can be searched for and is lowered on its own. ansible-core refuses such a key loudly, measured on 2.19.12: `[ERROR]: 'volant.loop' is not a valid attribute for a Block`, with the line. A playbook that uses an extension therefore never runs half-understood under ansible-core.

### The lowering contract

Each extension comes with a lowering rule, which rewrites it into plain Ansible. The extension's behaviour is defined as the behaviour of its lowering under ansible-core 2.19.12: Volant does not invent semantics, it runs the lowered form faster and keeps the original readable.

Each extension also comes with a round-trip test. The test runs the lowered form under ansible-core 2.19.12 and the extension under Volant, and compares the output and the recap. No extension merges without both.

### `volant eject`

```sh
volant eject [PATH...] [--out DIR] [--json]
```

It reads the playbooks and roles under `PATH` (the current directory by default) and writes each file that uses an extension with every extension lowered. With `--out DIR` the tree is written there; without it, the files are rewritten in place. Lowering may add files, such as the included task file of a lowered loop. They go under `_volant/` beside the file they come from, named after the task and a hash of its content, so that ejecting twice gives the same tree.

An extension whose lowering is not exact in a given context, such as `run_once` under the `free` strategy below, is refused with its `file:line` and the reason, and nothing is written. The report lists each file rewritten and each file added. `--json` gives one object per file: `path`, `action` (`rewritten` or `added`) and the extensions lowered. The exit codes are 0 when everything was lowered, 1 when an extension was refused, and 4 when a file cannot be read.

### `volant check --portable`

```sh
volant check --portable [PATH...] [--json]
```

It lists each line that ansible-core would refuse, with its `file:line` and the extension it uses. It exits 0 when there is none and 1 otherwise. `--json` gives one object per line: `path`, `line`, `extension`. A CI job can gate on it, and the output of `volant eject` must pass it.

### The first extensions

They are chosen by the public votes on ansible-core's issue tracker.

1. **A loop over a block** (ansible/ansible#13262), `volant.loop` with `volant.loop_control` on a block. The lowering moves the block's tasks to an included file and loops over the include:

   ```yaml
   - name: one account per user
     ansible.builtin.include_tasks: _volant/one-account-per-user-3f2a.yml
     loop: "{{ users }}"
   ```

   Measured on 2.19.12, with two hosts, under `linear` and under `serial: 1`: the includes are expanded first, then each item's tasks run in turn, each task across the hosts. A `when` inside applies per item, and `run_once` runs once per item. Under the `free` strategy, ansible-core does not honour `run_once` in the included file: it warns and runs the task on every host. A `run_once` inside a `volant.loop` under `free` is therefore refused by `eject` and reported by `check --portable`, as Volant follows the lowering.

2. **A block as a handler** (ansible/ansible#14270): a block in `handlers:` that a task can notify by the block's name. Measured on 2.19.12, notifying a block by its name fails with `[ERROR]: The requested handler 'restart app' was not found in either the main handlers list nor in the listening handlers list`. The lowering turns the block into a handler that includes its tasks:

   ```yaml
   handlers:
     - name: restart app
       ansible.builtin.include_tasks: _volant/restart-app-91c0.yml
   ```

   Measured on 2.19.12: the notified handler includes the file for every notified host and runs its tasks in order, as a block would. It needs no new key, since Volant accepts the block where ansible-core cannot use it, so it is spelled as plain Ansible and lowered all the same.

3. **`collection uninstall`** (ansible/ansible#67759), a command of `volant galaxy` rather than syntax. It changes nothing in a playbook, so it needs no lowering and never blocks an eject.

The others voted for are left for later records: `until` and `async` on a block (#46203, #28997) have no exact lowering that keeps the attempts and the polling, and a conditional `import_playbook` (#34281), a verbosity per task (#24215), AND and OR in tag expressions (#11396) and a list of addresses for `ansible_host` (proposals#97) each need their own measurement first.

### Shadow replays and the lock

A [shadow replay](/docs/start/switch-without-risk/) replays a recording of ansible-core, and ansible-core refuses an extension, so an extension never appears in a recording. A project that wants both keeps its playbooks portable while it records and replays, and starts using extensions once it has switched. Alternatively, it records the output of `volant eject` and replays that. The lock follows the same rule: it pins the engine that runs a project and says nothing about which extensions the project uses.

## Consequences

- A project can leave Volant at any time: `volant eject` gives it back a tree ansible-core runs, and `check --portable` proves it.
- Every extension costs a lowering rule and a round-trip test before it merges, and a context where the lowering is not exact is refused, not approximated.
- The implementation follows in these issues, by size:
  - parsing the `volant.` keys and refusing an unknown one (small);
  - the lowering pass, shared by `eject` and by the round-trip tests (medium);
  - `volant eject` (medium);
  - `volant check --portable` (small);
  - `volant.loop` and `volant.loop_control` on a block (medium);
  - a block as a handler (small);
  - `volant galaxy collection uninstall` (small).

## Measurements, 2026-10-25

ansible-core 2.19.12, two hosts over a local connection, `gather_facts: false`. The lowered loop includes three tasks, `create`, `once` (with `run_once: true`) and `key` (with `when: item != 'bob' or inventory_hostname == 'h1'`), over `[alice, bob]`.

Under `linear`, the includes come first and each item's tasks run in turn:

```text
TASK [per user] ****************************************************************
included: _volant/per-user.yml for h1, h2 => (item=alice)
included: _volant/per-user.yml for h1, h2 => (item=bob)
TASK [create] ******************************************************************
ok: [h1] => {"msg": "create alice on h1"}
ok: [h2] => {"msg": "create alice on h2"}
TASK [once] ********************************************************************
ok: [h1] => {"msg": "once alice"}
TASK [key] *********************************************************************
ok: [h1] => {"msg": "key alice"}
ok: [h2] => {"msg": "key alice"}
TASK [create] ******************************************************************
...
TASK [key] *********************************************************************
ok: [h1] => {"msg": "key bob"}
skipping: [h2]
```

Under `free`, `run_once` is not honoured:

```text
[WARNING]: Using run_once with the free strategy is not currently supported. This task will still be executed for every host in the inventory list.
TASK [once] ********************************************************************
ok: [h1] => {"msg": "once alice"}
TASK [once] ********************************************************************
ok: [h2] => {"msg": "once alice"}
```

Under `serial: 1`, each batch runs the whole loop, host after host. A block in `handlers:` cannot be notified by its name, and its lowering runs:

```text
TASK [change] ******************************************************************
[ERROR]: The requested handler 'restart app' was not found in either the main handlers list nor in the listening handlers list

RUNNING HANDLER [restart app] **************************************************
included: _volant/restart-app.yml for h1, h2
RUNNING HANDLER [stop] *********************************************************
ok: [h1] => {"msg": "stop"}
ok: [h2] => {"msg": "stop"}
RUNNING HANDLER [start] ********************************************************
ok: [h1] => {"msg": "start"}
ok: [h2] => {"msg": "start"}
```
