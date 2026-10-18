---
title: Filters, tests and lookups in Python
description: Which filters, tests and lookups need the controller's Python, how Volant finds it, and what fails without it.
---

Volant implements the common filters, tests and lookups in Rust. Every other one that ansible-core 2.19.12 or a collection ships runs in a Python helper on the controller, with the plugin's own code, so the result is the one ansible-core gives.

## What needs the helper

A name needs the helper when Volant has no Rust version of it. The [parity page](/reference/parity/) marks those rows `runs` with the note "through the Python helper; needs the plugin's Python libraries". Examples are the `hash`, `password_hash`, `json_query` and `ipaddr` filters, and the `lines` and `url` lookups.

A name with a Rust version never reaches the helper, whatever it is called. `ansible.utils.ipwrap` stays in Rust.

A play that uses only Rust filters, tests and lookups never starts Python for templating. The helper starts on the first bridged call and then serves the rest of the run.

A bridged plugin also needs what it imports. `json_query` needs `jmespath` and `ipaddr` needs `netaddr`, and a collection's plugin needs that collection, all installed for the interpreter the helper runs under. Without them the task fails with the plugin's own message.

The helper takes about 0.55 s to start. A run pays that once, on its first bridged call, measured with `hash`.

## What the helper receives

Volant renders every argument first and sends the result as data. The helper does not template it again, so a value that came from a managed host and still holds `{{ ... }}` stays plain text. Values marked `!unsafe` arrive as data too.

## How the interpreter is found

The helper is the same Python 3 with `ansible-core` that builds module payloads. Volant uses `VOLANT_PYTHON` when it is set, and nothing else. Otherwise it tries its [managed runtime](/reference/runtime/), then the active virtual environment, then `python3` on your `PATH`, and installs the managed runtime when none works.

An interpreter without `ansible-core` is refused by name, and the message says which interpreters were tried. A task that calls a bridged name fails with a message such as `filter 'hash' needs the Python helper, which could not start: …`, followed by the reason, and the run exits non-zero.

## Failures

Each of these fails the task that made the call, with the helper's message in the task's error:

- The helper exits or is killed during a render.
- A request or an answer is larger than the frame limit.
- A plugin answers bytes that are not UTF-8.
- The helper answers for another request.

The next bridged call in the run fails the same way instead of waiting.

## Not sent to the helper

The vault filters, tests and lookup never go to the helper: the vault password stays in the controller process. See [Vault](/variables/vault/).
