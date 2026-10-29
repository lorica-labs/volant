---
title: The managed runtime
description: The Python and ansible-core that Volant installs for itself, where they live, what they download, and how to manage them.
---

Volant needs a Python 3 with `ansible-core` on the controller for Python modules, fact gathering and the filters and lookups it does not implement itself. If the machine has none, Volant installs its own, once, and uses it from then on. You do not need ansible installed to run a playbook, nor for `volant doc`, `config`, `galaxy`, `pull` and `console`, which run ansible-core's own tools from this runtime.

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

Volant starts the interpreter with `-P`, so a file in the working directory, such as `ansible.py` or `json.py`, is never imported in place of the real module. `-P` needs Python 3.11 or later, which ansible-core 2.19 needs too. An interpreter of your own keeps `PYTHONPATH` and its user site. [`volant doc`](/docs/reference/cli/doc/) chooses its interpreter by this rule too, and runs ansible-core's own `ansible-doc` in it.

`volant runtime status` shows the choice the next run would make, with every candidate and where it came from. The [`runtime` setting](/docs/reference/configuration/#volant) changes this order.

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

`--collections` takes names, each with an optional `:version`, or a requirements file ending in `.yml` or `.yaml`. The collections go to `collections/` inside the runtime. Volant looks there after the paths in `ANSIBLE_COLLECTIONS_PATH` and `[defaults] collections_path`, so collections you installed yourself win. [`volant galaxy`](/docs/reference/cli/galaxy/) installs into the first entry of that path, `~/.ansible/collections` by default, and lists the runtime's directory last.

The install is Volant's own: `volant runtime install --collections` runs `volant galaxy collection install -p <runtime>/collections`, prints the same lines and needs no Python for that step.

The runtime holds ansible-core and its dependencies and nothing else. A collection whose plugins need another Python package, such as `jmespath` for `json_query`, does not work in it. For those, use your own environment with `VOLANT_PYTHON`.

## Optional wheel sets

Some connection plugins need Python packages ansible-core does not bring. Two sets of them are pinned for every platform, like the base wheels, and installed only when you ask:

| Set | Packages | Size on disk (Linux x86_64) |
|---|---|---|
| `windows` | pywinrm, pypsrp and their dependencies | about 7 MB |
| `network` | paramiko, ansible-pylibssh, ncclient and their dependencies | about 31 MB |

```sh
volant runtime install --extras windows
volant runtime install --extras windows,network
```

`--extras` installs the runtime first when it is missing, or adds the sets to the runtime already there without touching its base wheels. The wheels are checked against their pinned hashes and installed with `--require-hashes`, no index and no dependency resolution. `volant runtime status` lists the sets installed. ansible-pylibssh has no musl build, so the `network` set on Alpine and other musl machines has paramiko and ncclient only.

A run does not install a set. When a play's host connects with `winrm` or `psrp` and the controller's Python is the managed runtime without the `windows` set, the run stops before any connection, with exit code 4:

```text
host 'win1': the winrm connection needs the "windows" runtime extras: run volant runtime install --extras windows
```

`network_cli` and `netconf` ask for the `network` set the same way. With `VOLANT_PYTHON`, Volant leaves the interpreter as it is, and the plugin's own error names the package it misses.

From the `windows` set, `winrm` works with the `basic`, `plaintext`, `ssl`, `certificate` and `ntlm` transports, and `psrp` with the `basic`, `certificate`, `ntlm` and `negotiate` authentications, where `negotiate` falls back to NTLM. Kerberos and CredSSP need system libraries and packages the set does not hold: use your own environment with `VOLANT_PYTHON` for them.

## Commands

| Command | What it does |
|---|---|
| `volant runtime install` | Installs the runtime if it is missing. |
| `volant runtime install --collections NAME_OR_FILE...` | Also installs collections. |
| `volant runtime install --extras SET,...` | Also installs optional wheel sets: `windows`, `network`. |
| `volant runtime install --from FILE` | Installs from a file made by `pack`, with no download. |
| `volant runtime install --from FILE --sha256 HASH` | Installs from the file only when its SHA-256 is `HASH`. |
| `volant runtime pack FILE` | Writes the installed runtime to a file and prints its SHA-256. |
| `volant runtime pack FILE --extras SET,...` | Adds the sets to the runtime first, so the file carries them. |
| `volant runtime status` | Shows the mode, the interpreter the next run uses, and the managed runtime. |
| `volant runtime path` | Prints the managed runtime's Python. Exits with code 1 when it is not installed. |
| `volant runtime remove [ID]` | Deletes the current runtime, or the one named. |
| `volant runtime remove --all` | Deletes every runtime. |

### Machines without a network

Pack a runtime on a machine that has a network, copy the file, and install it on a machine of the same platform:

```sh
volant runtime pack runtime.tar.gz
# packed runtime.tar.gz
# sha256 3f1c...e9
volant runtime install --from runtime.tar.gz --sha256 3f1c...e9
```

`install --from` refuses a file made for another platform or another runtime, and names both. `pack` prints the SHA-256 of the file on a `sha256` line. With `--sha256 HASH`, `install --from` refuses a file whose hash differs, before unpacking it, and shows both hashes. Without it the install prints the hash of the file it used, so you can compare it yourself. Installing a pack runs the Python inside it, so pass the hash you got from the machine that made it.

The file carries the sets the runtime held. Pack with `--extras` to add them, and name them again on the other machine, where `install --from` checks the file holds them:

```sh
volant runtime pack runtime.tar.gz --extras windows
volant runtime install --from runtime.tar.gz --sha256 3f1c...e9 --extras windows
```

## Turning it off

Set `runtime = system` in the `[volant]` section of `ansible.cfg`, or `VOLANT_RUNTIME=system`. Volant then uses only what was there before, and a missing interpreter is refused with the old message plus a hint about the runtime. With `managed`, Volant uses only `VOLANT_PYTHON` or the managed runtime, installing the runtime when it is missing, and ignores `python3` on the `PATH`. See [Configuration](/docs/reference/configuration/#volant).
