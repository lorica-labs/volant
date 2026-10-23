---
title: What is not supported yet
description: The pre-flight check that names what this release does not support yet, before it touches a host.
---

Volant's loader accepts the whole ansible-core 2.19 grammar, so a playbook parses here as it parses under Ansible. A second check, the pre-flight, then looks for everything the loader accepted and this release does not support yet. If it finds any, it names each one and stops the run before the first connection, before any banner and before any recap.

Without it, a keyword that slipped past both checks would be ignored in silence, and the run would report success after skipping what you asked for.

```mermaid
flowchart TB
  L[load the playbook, full 2.19 grammar] --> P1[pre-flight on the playbook as written]
  P1 --> C[compile roles and imports]
  C --> P2[pre-flight on the compiled steps]
  P2 --> R[connect and run]
  P1 -. not supported yet .-> X[exit 4, nothing ran]
  P2 -. not supported yet .-> X
  class P1,P2 key
  class X quiet
```

The check runs twice: once on the playbook as written, and once on the compiled steps. The first pass reads the play as written, so a task that `--tags` would have dropped still stops the run when it uses something not supported yet. The second pass reads the compiled steps, after tag selection, and is the only place where a keyword written inside a role or an imported file is visible. This errs toward stopping more often, never less. Every section of a block is checked, `rescue` and `always` included.

What a dynamic include brings in is checked when a host reaches it. A refusal there fails the include for that host, and a `rescue` or `ignore_errors` takes it. See [Includes and imports](/playbooks/includes/#dynamic-includes).

## What it refuses today

| What | Message shape | Exit code |
|---|---|---|
| A `strategy` other than `linear`, `free`, `host_pinned` and `debug` | `strategy 'x' is not supported yet` | 4 |
| `until`, `retries` or `delay` on `include_tasks` or `include_role` | `keyword 'until' is not supported yet on 'include_tasks'` | 4 |
| A `pause` with a `prompt` and `seconds` or `minutes`, when standard input is a terminal | `pause cannot wait for a prompt and a duration together on a terminal ...` | 4 |
| A native module argument that ansible-core has and this release does not act on | `argument 'x' is not supported yet on 'module'`, with ` with 'v'` when only that value is refused | 4 |
| `end_role` outside a role | `Cannot execute 'end_role' from outside of a role` | 4 |
| A builtin module this release does not carry | `module 'x' is not available in this release` | 4 |
| A string argument that is one template | `module 'x' takes its arguments from one template ... write them as a mapping` | 4 |
| A module or collection that does not resolve | `couldn't resolve module/action 'x' ...`, with the install command for a missing collection | 4 |
| A collection module that needs an action plugin for Windows or a network device | `module 'x' needs an action plugin from its collection, which this release does not run` | 4 |
| A `become_method` other than `sudo` and `su` | `become_method 'x' is not supported yet` | 2 |
| A role in `roles:` or `import_role` that nobody can find | `the role 'x' was not found in ...` | 1 |

The `pause` refusal applies only to a step that always runs. A `when`, a `loop` or a `rescue` around it leaves the check to the moment the `pause` is reached.

## Exit codes

A run the pre-flight stops exits with code 4, which is what Ansible uses for a playbook it cannot load. Some cases keep the code Ansible was measured to use:

| Case | Exit code |
|---|---|
| Anything this release does not support yet | 4 |
| A role that `roles:` or `import_role` cannot find | 1 |
| An escalation method this release does not support yet | 2 |

Three cases are no longer pre-flight cases. They fail when the task is reached, as in ansible-core:

- An unknown `meta` action prints its banner, then `invalid meta action requested: <action>` and exits 1, whatever its `when` says.
- A `notify` that names no handler ends the run with exit 1 when the task reports `changed`. See [Handlers](/playbooks/handlers/#differences-from-ansible).
- An `include_role` of a role whose task file does not load runs the plays and tasks before it, then ends with exit 4 after the statement's banner. A `when: false` on the include hides the failure.

[Exit codes](/reference/exit-codes/) lists every code Volant uses.

Listings and `--syntax-check` never call the pre-flight, so you can read a playbook before the release that runs it ships.

## Differences from Ansible

- `tags` and `timeout` are checked before their value is looked at, so `tags: 3` does not get Ansible's type error.
- A `with_<name>` that is no lookup plugin of the controller's ansible-core is refused with the reference's sentence, `'with_xyz' is not a valid attribute for a Task`. In a file that `include_tasks` or `include_role` reads while the play runs, the include fails for the host that reached it, and the play does not stop at exit 4.
- For a module this release does not support yet, the arguments are not parsed, so the error is about the module and not about a `key=value` on the next line.
- `gather_facts`, `ignore_errors` and `become` written as something other than true or false are rejected when the playbook loads, one step earlier than Ansible rejects them, with the reference's message and exit code. A template is not rejected: `ignore_errors: "{{ ... }}"` renders when the task runs, for each loop item, and the [templated keywords](/playbooks/how-a-play-runs/#templated-keywords) render for each host at the same moment. `gather_facts` and `force_handlers` of a play render when the play starts. All of them are read with the reference's rules, and a value they cannot take fails the task, or stops the play for the last two, with the reference's message.
