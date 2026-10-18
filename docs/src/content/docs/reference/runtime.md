---
title: The managed runtime
description: The Python and ansible-core that Volant installs for itself, where they live, what they download, and how to manage them.
---

Volant needs a Python 3 with `ansible-core` on the controller for Python modules, fact gathering and the filters and lookups it does not implement itself. If the machine has none, Volant installs its own, once, and uses it from then on. You do not need ansible installed to run a playbook.

The runtime is CPython 3.13 and ansible-core 2.19.12, the version Volant matches. It lives in one directory that Volant owns, so it touches no system Python and no virtual environment of yours.

## When it is installed

A run installs the runtime when it needs the Python helper and finds no interpreter it can use. It prints one line on standard error and goes on:

```text
Installing Volant's Python runtime (cpython-3.13.16-ansible-core-2.19.12-98a5d8b2) ...
```

A run that only gathers facts never installs anything. You can also install the runtime ahead of time, for example in an image:

```sh
volant runtime install
```

On a clean machine with a normal connection the install took about 8 seconds. Later runs find it with two file checks.

## Which interpreter a run uses

The first of these that applies:

1. `VOLANT_PYTHON`, when it is set. Volant uses exactly that interpreter and never installs.
2. The managed runtime, when it is installed.
3. The active virtual environment, then `python3` on your `PATH`, when they have ansible-core.
4. A new managed runtime, installed now.

`volant runtime status` shows the choice the next run would make, with every candidate and where it came from. The [`runtime` setting](/reference/configuration/#volant) changes this order.

## What it downloads

Volant fetches a standalone CPython build and the ansible-core wheels with `curl`, or `wget` when `curl` is missing. It also runs `tar`. The URLs and the SHA-256 of every file are pinned inside the Volant binary, so each release knows exactly which files it may use.

- Each download is checked against its pinned hash before anything in it runs. On a mismatch Volant deletes the download and stops with both hashes.
- `pip` installs only the pinned wheels, with `--require-hashes` and no index.
- The install happens in a temporary directory next to the final one and is moved into place after a check that `import ansible` works. A failed install removes its temporary directory. An interrupted one can leave a `.<id>.partial-*` directory, which the next install deletes.
- A lock file keeps two Volant processes from installing at once. The second waits and then finds the runtime installed.
- Every child process runs with `PYTHONNOUSERSITE=1` and without `PYTHONPATH`, so packages in your home directory or your environment cannot reach it.

The platforms are Linux and macOS on x86_64 and arm64. On Linux, Volant reads the libc of the machine it runs on, glibc or musl, and picks the matching build, whatever it was compiled for. Volant has not been tested on macOS: no Mac was available. Any other system is refused with a message that names it and points to `VOLANT_PYTHON`.

## Where it lives

Under `$XDG_DATA_HOME/volant/runtime`, or `~/.local/share/volant/runtime`. On macOS the default is `~/Library/Application Support/volant/runtime`. Each runtime has its own directory, named by its identifier (Python version, ansible-core version and a short hash of the pinned file list), so a release with other pins installs next to the old one. A runtime takes about 155 MB.

## Collections

Volant does not install a collection during a run. To install one into the runtime:

```sh
volant runtime install --collections community.general ansible.posix:2.2.2
volant runtime install --collections requirements.yml
```

`--collections` takes names, each with an optional `:version`, or a requirements file ending in `.yml` or `.yaml`. The collections go to `collections/` inside the runtime. Volant looks there after the paths in `ANSIBLE_COLLECTIONS_PATH` and `[defaults] collections_path`, so collections you installed yourself win.

The runtime holds ansible-core and its dependencies and nothing else. A collection whose plugins need another Python package, such as `jmespath` for `json_query`, does not work in it. For those, use your own environment with `VOLANT_PYTHON`.

## Commands

| Command | What it does |
|---|---|
| `volant runtime install` | Installs the runtime if it is missing. |
| `volant runtime install --collections NAME_OR_FILE...` | Also installs collections. |
| `volant runtime install --from FILE` | Installs from a file made by `pack`, with no download. |
| `volant runtime pack FILE` | Writes the installed runtime to a file. |
| `volant runtime status` | Shows the mode, the interpreter the next run uses, and the managed runtime. |
| `volant runtime path` | Prints the managed runtime's Python. Exits with code 1 when it is not installed. |
| `volant runtime remove [ID]` | Deletes the current runtime, or the one named. |
| `volant runtime remove --all` | Deletes every runtime. |

### Machines without a network

Pack a runtime on a machine that has a network, copy the file, and install it on a machine of the same platform:

```sh
volant runtime pack runtime.tar.gz
volant runtime install --from runtime.tar.gz
```

`install --from` refuses a file made for another platform or another runtime, and names both. The file is not checked against a hash: it is trusted like any file you copy yourself, and installing it runs the Python inside it. Copy it over a channel you trust.

## Turning it off

Set `runtime = system` in the `[volant]` section of `ansible.cfg`, or `VOLANT_RUNTIME=system`. Volant then uses only what was there before, and a missing interpreter is refused with the old message plus a hint about the runtime. With `managed`, Volant uses only `VOLANT_PYTHON` or the managed runtime, installing the runtime when it is missing, and ignores `python3` on the `PATH`. See [Configuration](/reference/configuration/#volant).
