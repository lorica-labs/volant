# SPDX-License-Identifier: GPL-3.0-or-later
"""Runs every case in cases.yml through the reference ansible-playbook and records what it did.

The interpreter comes from the ansible-core tool environment, so PyYAML is available.
"""
import base64
import gzip
import grp
import io
import json
import os
import pwd
import re
import shlex
import shutil
import socket
import subprocess
import sys
import tarfile
import tempfile

import yaml

HERE = os.path.dirname(os.path.abspath(__file__))
REFERENCE = open(os.path.join(HERE, "ANSIBLE_VERSION"), encoding="utf-8").read().strip()


def _read_pins(name):
    pins = {}
    with open(os.path.join(HERE, name), encoding="utf-8") as f:
        for line in f:
            line = line.strip()
            if line:
                key, version = line.split()
                pins[key] = version
    return pins


# The collection versions this golden is recorded against, plus `netaddr`: not a collection, but
# pinned the same way because `ansible.utils.ipwrap` needs it in the same controller environment
# and a mismatched one would silently change what a filter measured against it returns.
COLLECTIONS = _read_pins("COLLECTIONS")


def _installed_collections():
    """What the controller's own ansible-core actually has, read the way it reads it: a
    collection's version comes from its own `MANIFEST.json`, on whichever of its configured
    collection paths carries that collection; `netaddr` is a plain import next to it."""
    from ansible import constants as ansible_constants

    versions = {}
    for fqcn in COLLECTIONS:
        if fqcn == "netaddr":
            continue
        namespace, name = fqcn.split(".", 1)
        for root in ansible_constants.COLLECTIONS_PATHS:
            manifest = os.path.join(os.path.expanduser(root), "ansible_collections", namespace, name, "MANIFEST.json")
            if os.path.exists(manifest):
                with open(manifest, encoding="utf-8") as f:
                    versions[fqcn] = json.load(f)["collection_info"]["version"]
                break
    try:
        import netaddr

        versions["netaddr"] = netaddr.__version__
    except ImportError:
        pass
    return versions


def collection_mismatches():
    """What in COLLECTIONS the controller does not actually have installed, one line per pin."""
    installed = _installed_collections()
    return [
        f"{name} is {installed.get(name, 'not installed')}, not {version}"
        for name, version in COLLECTIONS.items()
        if installed.get(name) != version
    ]


def main() -> int:
    version = subprocess.run(["ansible-playbook", "--version"], capture_output=True, text=True, check=True).stdout
    if REFERENCE not in version.splitlines()[0]:
        print(f"ansible-playbook is not {REFERENCE}: {version.splitlines()[0]}", file=sys.stderr)
        return 1
    mismatched = collection_mismatches()
    if mismatched:
        print("collections do not match COLLECTIONS: " + "; ".join(mismatched), file=sys.stderr)
        return 1
    with open(os.path.join(HERE, "cases.yml"), encoding="utf-8") as f:
        cases = yaml.safe_load(f)
    tasks = []
    payloads = {}
    for i, case in enumerate(cases):
        # A case naming `untrusted` needs those variables to have come from the host rather than
        # from the playbook, which is what the reference decides trust on. Each one is read out of
        # a file by `command`: a literal in the task's own text would be rendered by the reference
        # before the command ever ran.
        #
        # The register carries the case's number and the case's own `vars:` alias it under the
        # name cases.yml wrote. A register outlives the task that set it, so a name written plain
        # would stay defined for every case after this one, and a later case expecting it to be
        # undefined would read this stale value and record the wrong reference answer.
        case_vars = dict(case.get("vars") or {})
        for name, text in case.get("untrusted", {}).items():
            payloads[f"{i}-{name}"] = text
            register = f"raw_{i}_{name}"
            tasks.append({
                "name": f"setup {i} {name}",
                "command": "cat {{ payload_dir }}/" + f"{i}-{name}.txt",
                "register": register,
            })
            case_vars[name] = f"{{{{ {register}.stdout }}}}"
        task = {"name": f"case {i}", "ignore_errors": True}
        if case_vars:
            task["vars"] = case_vars
        if "when" in case:
            task["debug"] = {"msg": "ran"}
            task["when"] = case["when"]
        else:
            task["debug"] = {"msg": case["template"]}
        tasks.append(task)
    play = [{"hosts": "localhost", "gather_facts": False, "connection": "local", "tasks": tasks}]
    env = dict(os.environ, ANSIBLE_STDOUT_CALLBACK="ansible.builtin.json", ANSIBLE_NOCOLOR="1")
    env["VOLANT_GOLDEN_ENV"] = "golden-env-value"
    with tempfile.TemporaryDirectory() as tmp:
        play[0]["vars"] = {"payload_dir": tmp}
        for stem, text in payloads.items():
            with open(os.path.join(tmp, f"{stem}.txt"), "w", encoding="utf-8") as f:
                f.write(text)
        playbook = os.path.join(tmp, "golden.yml")
        with open(playbook, "w", encoding="utf-8") as f:
            # sort_keys=False for the same reason as the json.dump below: safe_dump sorts every
            # mapping by default, which would hand the reference a case's `vars` in an order
            # cases.yml never wrote and silently make key order untestable.
            yaml.safe_dump(play, f, default_flow_style=False, allow_unicode=True, sort_keys=False)
        with open(os.path.join(tmp, "golden_lookup.txt"), "w", encoding="utf-8") as f:
            f.write("file contents")
        run = subprocess.run(["ansible-playbook", "-i", "localhost,", playbook], env=env, capture_output=True, text=True)
    report = json.loads(run.stdout)
    # By name rather than by position: a case that needs setup tasks in front of it puts them in
    # the same play, and zipping would then read one case's answer off another case's task.
    outcomes = {t["task"]["name"]: t["hosts"]["localhost"] for t in report["plays"][0]["tasks"]}
    results = []
    for i, case in enumerate(cases):
        outcome = outcomes[f"case {i}"]
        entry = {"case": case}
        if outcome.get("skipped"):
            entry["skipped"] = True
        elif outcome.get("failed"):
            # The playbook is written into a fresh temporary directory, and a few of the
            # reference's messages quote the file they came from. Left in, those lines carry a
            # different random path on every run, so regenerating a tree that is already correct
            # produces a diff and "regenerate and see nothing change" cannot be used as a check.
            error = outcome.get("msg", "")
            entry["error"] = error.replace(tmp, "<tmpdir>") if isinstance(error, str) else error
        else:
            entry["result"] = outcome.get("msg")
        results.append(entry)
    # Deliberately not sort_keys: a case's `vars` must reach the Rust side in the order
    # cases.yml wrote them, or no case could measure what a filter does to key order.
    with open(os.path.join(HERE, "expected.json"), "w", encoding="utf-8") as f:
        json.dump(results, f, indent=2, ensure_ascii=False)
        f.write("\n")
    print(f"{len(results)} cases recorded against ansible-core {REFERENCE}")
    # action_plugins() is the most environment-sensitive: `package` and `service` need `become`
    # and a real package manager or systemd. collection_modules() needs the pinned collections
    # instead, checked above before any of this ran. Both go after the pre-existing, unrelated
    # goldens so a failure in either cannot also cost those. natives() goes last: it is the only
    # one that creates an account and a group on the machine.
    return (
        inventory()
        or listings()
        or python_modules()
        or action_plugins()
        or collection_modules()
        or natives()
    )


# A fixed, non-temporary path: `stat`, `file` and `lineinfile` all read or write under it, and
# `file`/`lineinfile` are idempotent in a way that would leak into their recorded result if the
# directory carried anything over from a previous run (lineinfile's "line added" becomes "no
# change" once the line is already there), so python_modules() clears it before every run rather
# than only creating it once.
PYTHON_MODULES_TMP = "/tmp/volant15-golden"
STAT_TARGET = os.path.join(PYTHON_MODULES_TMP, "golden-stat-target")
STAT_TARGET_CONTENT = "golden stat fixture\n"
STAT_TARGET_MODE = 0o644

# Values that identify the machine that ran the generator rather than anything a module
# returned. file's owner/group/uid/gid and stat's pw_name/gr_name/uid/gid all name the account
# that happened to run the generator, which must never appear in the repository (stat's, unlike
# /etc/hostname's, is not root: STAT_TARGET is a file this script itself creates, so it is owned
# by whoever ran it). The placeholders keep the type ansible actually returns (str for the two
# name keys, int for the two id keys), because the Rust comparison these fixtures feed checks
# these keys by presence and type rather than by value.
ACCOUNT_NAME_PLACEHOLDER = "<golden-generator-account>"
ACCOUNT_ID_PLACEHOLDER = 999999999


def _redact_account(container, name_keys, id_keys):
    for key in name_keys:
        if key in container:
            container[key] = ACCOUNT_NAME_PLACEHOLDER
    for key in id_keys:
        if key in container:
            container[key] = ACCOUNT_ID_PLACEHOLDER


def python_modules():
    """Record raw results from selected Python modules on localhost."""
    modules = [
        {"name": "ping", "module": "ping", "args": {}},
        # /etc/hostname's own checksum is sha1(hostname), a confirmable hash of an
        # infrastructure host name; STAT_TARGET is written by this script with fixed content, so
        # the checksum is reproducible on every machine and identifies nothing.
        {"name": "stat", "module": "stat", "args": {"path": STAT_TARGET}},
        {
            "name": "file",
            "module": "file",
            # An explicit mode, not the umask-dependent default: without it the recorded value
            # names the generating account's umask rather than anything the module did.
            "args": {"path": f"{PYTHON_MODULES_TMP}/golden-file", "state": "touch", "mode": "0644"},
        },
        {
            "name": "lineinfile",
            "module": "lineinfile",
            "args": {"path": f"{PYTHON_MODULES_TMP}/golden-line", "line": "hello", "create": True},
        },
        {
            "name": "apt",
            "module": "apt",
            # A package that is not installed, in check mode: needs no root (nothing is written,
            # by construction of check mode) and pins that the module actually consulted the apt
            # cache and found the target absent (`changed: true`). `name=bash, state=present`
            # records `changed: false` on every Debian host because bash is always already
            # there, which is the emptiest possible result: a Volant `apt` that consults nothing
            # and hands back `{"changed": false}` would pass that comparison green. ignore_errors
            # plus check_mode below turn any failure here (no apt/apt_pkg bindings, an empty
            # package cache) into the same "skip this one module" path a missing apt-get takes,
            # rather than into a failed generation.
            "args": {"name": "cowsay", "state": "present"},
            "task_extra": {"check_mode": True, "ignore_errors": True},
        },
    ]

    tasks = []
    for m in modules:
        task = {"name": m["name"], m["module"]: m["args"]}
        task.update(m.get("task_extra", {}))
        tasks.append(task)
    play = [{"hosts": "localhost", "gather_facts": False, "connection": "local", "tasks": tasks}]
    env = dict(
        os.environ,
        ANSIBLE_STDOUT_CALLBACK="ansible.builtin.json",
        ANSIBLE_NOCOLOR="1",
        # An explicit interpreter, not just a quiet discovery mode: `auto_silent` still leaves
        # ansible_facts.discovered_interpreter_python naming this machine's python, it only
        # silences the warning that goes with it (measured). Naming the interpreter outright
        # skips discovery altogether, so neither the warning nor the fact is ever produced, and
        # this generator has one less machine-specific value to filter out after the fact. Every
        # host this generator supports already needs apt, so it already needs to be Debian
        # family, where this path is the system python.
        ANSIBLE_PYTHON_INTERPRETER="/usr/bin/python3",
    )
    shutil.rmtree(PYTHON_MODULES_TMP, ignore_errors=True)
    os.makedirs(PYTHON_MODULES_TMP, exist_ok=True)
    with open(STAT_TARGET, "w", encoding="utf-8") as f:
        f.write(STAT_TARGET_CONTENT)
    # An explicit mode for the same reason file's task carries one: stat reports the target's
    # mode and the twelve permission bits read from it, and left to the umask those name whoever
    # ran this script. golden.rs sets the same mode on its own copy.
    os.chmod(STAT_TARGET, STAT_TARGET_MODE)
    playbook =os.path.join(PYTHON_MODULES_TMP, "python-modules.yml")
    with open(playbook, "w", encoding="utf-8") as f:
        yaml.safe_dump(play, f, default_flow_style=False, allow_unicode=True, sort_keys=False)
    run = subprocess.run(
        ["ansible-playbook", "-i", "localhost,", playbook],
        env=env,
        capture_output=True,
        text=True,
    )
    if run.returncode:
        print(
            f"ansible-playbook exited {run.returncode} while recording the python modules",
            file=sys.stderr,
        )
        if run.stderr:
            print(run.stderr, file=sys.stderr)
        return 1
    report = json.loads(run.stdout)
    outcomes = {task["task"]["name"]: task["hosts"]["localhost"] for task in report["plays"][0]["tasks"]}
    destination = os.path.join(HERE, "python-modules")
    os.makedirs(destination, exist_ok=True)
    recorded = 0
    for m in modules:
        name = m["name"]
        outcome = outcomes[name]
        if name == "apt" and outcome.get("failed"):
            print(f"apt is not usable here: {outcome.get('msg', 'unknown error')}", file=sys.stderr)
            stale = os.path.join(destination, "apt.json")
            if os.path.exists(stale):
                os.remove(stale)
                print(
                    "removed the stale apt.json so a later contributor does not read it as "
                    "verified against this reference",
                    file=sys.stderr,
                )
            else:
                print("no apt.json to remove; nothing was recorded for apt", file=sys.stderr)
            continue
        result = dict(outcome)
        result.pop("invocation", None)
        _redact_account(result, ("owner", "group"), ("uid", "gid"))
        stat_info = result.get("stat")
        if isinstance(stat_info, dict):
            _redact_account(stat_info, ("pw_name", "gr_name"), ("uid", "gid"))
        with open(os.path.join(destination, f"{name}.json"), "w", encoding="utf-8") as f:
            json.dump(result, f, indent=2, ensure_ascii=False, sort_keys=True)
            f.write("\n")
        recorded += 1
    print(f"python module goldens recorded: {recorded}/{len(modules)} modules")
    return 0


# A fixed, non-temporary path, cleared at the top of every run: idempotence is exactly what a
# few of these cases measure (`copy-same`, `template-same`), so a directory left over from a
# previous pass would turn the second run's "changed" into "no change" and record the wrong
# reference answer.
ACTION_TMP = "/tmp/volant16-golden"
ACTION_SRC = os.path.join(HERE, "action-src")

# The path a staged copy lands under on the controller before the reference pushes it to the
# managed node, and the equivalent for `connection: local`. Both change on every run (a random
# suffix, and it is rooted under the generating account's home directory), so any string that
# carries one is replaced by a fixed placeholder, wherever it turns up: `unarchive`'s own
# `extract_results.cmd` quotes it as one argument of the `tar` command line it ran, not only
# under the `src` key.
STAGED_SRC_MARKERS = ("ansible-tmp-", "/tmp/ansible", ".ansible/tmp")


def _redact_staged_paths(value):
    if isinstance(value, str):
        if any(marker in value for marker in STAGED_SRC_MARKERS):
            return "<golden-staged-path>"
        return value
    if isinstance(value, list):
        return [_redact_staged_paths(v) for v in value]
    if isinstance(value, dict):
        return {k: _redact_staged_paths(v) for k, v in value.items()}
    return value


STATUS_VALUE_PLACEHOLDER = "<golden-systemd-value>"


def _redact_service_status(result):
    # `service`'s `status` is the reference's own live `systemctl show` of the unit:
    # ActiveEnterTimestamp, CPUUsageNSec, MemoryCurrent, InvocationID and every other value in
    # it change on every query (a timestamp, a running counter, a per-boot id), which is exactly
    # what would make two consecutive `just golden` runs disagree on this file. The golden test
    # compares `status` by its keys only, so the keys are kept and every value replaced by one
    # fixed placeholder.
    status = result.get("status")
    if isinstance(status, dict):
        for key in status:
            status[key] = STATUS_VALUE_PLACEHOLDER


def _write_bundle(path):
    """A tar.gz whose bytes are the same on every machine and every run: a fixed member mtime,
    owner and name, wrapped in a gzip header that carries none of gzip's own defaults (its
    mtime, and a filename it would otherwise take from the destination path)."""
    buf = io.BytesIO()
    with tarfile.open(fileobj=buf, mode="w") as tar:
        content = b"inside\n"
        info = tarfile.TarInfo(name="inside.txt")
        info.size = len(content)
        info.mtime = 0
        info.uid = 0
        info.gid = 0
        info.uname = ""
        info.gname = ""
        tar.addfile(info, io.BytesIO(content))
    with open(path, "wb") as raw:
        with gzip.GzipFile(fileobj=raw, filename="", mtime=0, mode="wb") as gz:
            gz.write(buf.getvalue())


def _ensure_action_fixtures():
    """The three inputs `action_plugins()` acts on, written once: once committed, a fixed
    content is the point, so a fixture already on disk is left untouched rather than
    regenerated."""
    os.makedirs(ACTION_SRC, exist_ok=True)
    hello = os.path.join(ACTION_SRC, "hello.txt")
    if not os.path.exists(hello):
        with open(hello, "w", encoding="utf-8") as f:
            f.write("hello\n")
        os.chmod(hello, 0o644)
    motd = os.path.join(ACTION_SRC, "motd.j2")
    if not os.path.exists(motd):
        with open(motd, "w", encoding="utf-8") as f:
            f.write(
                "# {{ ansible_managed }}\nhost={{ who }}\n{% if extra %}\nextra=yes\n{% endif %}\n"
            )
        os.chmod(motd, 0o644)
    bundle = os.path.join(ACTION_SRC, "bundle.tar.gz")
    if not os.path.exists(bundle):
        _write_bundle(bundle)
        os.chmod(bundle, 0o644)
    return hello, motd, bundle


def action_plugins():
    """Record raw results from the action plugins that do real work on a managed host: copy,
    template, package, service and unarchive.

    `gather_facts: false` is deliberate: it is the path through which `package` and `service`
    run their own filtered `setup`, and that path is the one Volant has to reproduce.
    """
    hello, motd, bundle = _ensure_action_fixtures()
    dest_dir = f"{ACTION_TMP}/dir"
    unpacked_dir = f"{ACTION_TMP}/unpacked"

    tasks = []
    recorded = []

    def add(name, module, args, extra=None):
        task = {"name": name, module: args}
        task.update(extra or {})
        tasks.append(task)
        recorded.append(name)

    def setup(module, args):
        tasks.append({"name": f"setup-{len(tasks)}", module: args})

    add(
        "copy-new",
        "copy",
        {"src": hello, "dest": f"{ACTION_TMP}/new.txt", "mode": "0644"},
    )
    add(
        "copy-same",
        "copy",
        {"src": hello, "dest": f"{ACTION_TMP}/new.txt", "mode": "0644"},
    )
    add(
        "copy-content",
        "copy",
        {"content": "x\n", "dest": f"{ACTION_TMP}/content.txt", "mode": "0644"},
    )
    add(
        "copy-force-false",
        "copy",
        {"content": "y\n", "dest": f"{ACTION_TMP}/content.txt", "force": False},
    )
    # An explicit mode on both directories: `unarchive-local` reports its destination's, and
    # left to the umask it would name whoever ran the generator.
    setup("file", {"path": dest_dir, "state": "directory", "mode": "0775"})
    add(
        "copy-dest-dir",
        "copy",
        {"src": hello, "dest": f"{dest_dir}/", "mode": "0644"},
    )
    add(
        "copy-validate-fail",
        "copy",
        {"content": "", "dest": f"{ACTION_TMP}/invalid.txt", "validate": "test -s %s"},
        {"ignore_errors": True},
    )
    add(
        "copy-remote-src",
        "copy",
        {
            "src": f"{ACTION_TMP}/new.txt",
            "dest": f"{ACTION_TMP}/remote.txt",
            "remote_src": True,
            "mode": "0644",
        },
    )
    template_vars = {"vars": {"who": "golden", "extra": True}}
    add(
        "template-new",
        "template",
        {"src": motd, "dest": f"{ACTION_TMP}/motd", "mode": "0644"},
        template_vars,
    )
    add(
        "template-same",
        "template",
        {"src": motd, "dest": f"{ACTION_TMP}/motd", "mode": "0644"},
        template_vars,
    )
    add(
        "template-lstrip",
        "template",
        {
            "src": motd,
            "dest": f"{ACTION_TMP}/motd-lstrip",
            "mode": "0644",
            "lstrip_blocks": True,
        },
        template_vars,
    )
    # Both need `become` to do anything real, and neither is guaranteed on a contributor's
    # machine (no `sudo -n`, no systemd): `ignore_errors` keeps a failure here from also halting
    # the unrelated cases that follow, the same role it plays for `apt` in python_modules().
    add(
        "package-present",
        "package",
        {"name": "bash", "state": "present"},
        {"become": True, "ignore_errors": True},
    )
    add(
        "service-started",
        "service",
        {"name": "systemd-journald", "state": "started"},
        {"become": True, "ignore_errors": True},
    )
    setup("file", {"path": unpacked_dir, "state": "directory", "mode": "0775"})
    add("unarchive-local", "unarchive", {"src": bundle, "dest": unpacked_dir})
    add(
        "unarchive-creates",
        "unarchive",
        {
            "src": bundle,
            "dest": unpacked_dir,
            "creates": f"{unpacked_dir}/inside.txt",
        },
    )

    play = [{"hosts": "localhost", "gather_facts": False, "connection": "local", "tasks": tasks}]
    env = dict(
        os.environ,
        ANSIBLE_STDOUT_CALLBACK="ansible.builtin.json",
        ANSIBLE_NOCOLOR="1",
        # Same reasoning as python_modules(): naming the interpreter outright skips discovery,
        # so neither its warning nor the fact it would set ever reaches these results.
        ANSIBLE_PYTHON_INTERPRETER="/usr/bin/python3",
    )
    shutil.rmtree(ACTION_TMP, ignore_errors=True)
    os.makedirs(ACTION_TMP, exist_ok=True)
    playbook = os.path.join(ACTION_TMP, "action-plugins.yml")
    with open(playbook, "w", encoding="utf-8") as f:
        yaml.safe_dump(play, f, default_flow_style=False, allow_unicode=True, sort_keys=False)
    run = subprocess.run(
        ["ansible-playbook", "-i", "localhost,", playbook],
        env=env,
        capture_output=True,
        text=True,
    )
    if run.returncode:
        print(
            f"ansible-playbook exited {run.returncode} while recording the action plugins",
            file=sys.stderr,
        )
        if run.stderr:
            print(run.stderr, file=sys.stderr)
        return 1
    report = json.loads(run.stdout)
    outcomes = {t["task"]["name"]: t["hosts"]["localhost"] for t in report["plays"][0]["tasks"]}
    destination = os.path.join(HERE, "action")
    os.makedirs(destination, exist_ok=True)
    count = 0
    for name in recorded:
        outcome = outcomes[name]
        if name in ("package-present", "service-started") and outcome.get("failed"):
            print(f"{name} is not usable here: {outcome.get('msg', 'unknown error')}", file=sys.stderr)
            stale = os.path.join(destination, f"{name}.json")
            if os.path.exists(stale):
                os.remove(stale)
                print(
                    f"removed the stale {name}.json so a later contributor does not read it as "
                    "verified against this reference",
                    file=sys.stderr,
                )
            else:
                print(f"no {name}.json to remove; nothing was recorded for {name}", file=sys.stderr)
            continue
        result = dict(outcome)
        result.pop("invocation", None)
        _redact_account(result, ("owner", "group"), ("uid", "gid"))
        result = _redact_staged_paths(result)
        if name == "service-started":
            _redact_service_status(result)
        with open(os.path.join(destination, f"{name}.json"), "w", encoding="utf-8") as f:
            json.dump(result, f, indent=2, ensure_ascii=False, sort_keys=True)
            f.write("\n")
        count += 1
    print(f"action plugin goldens recorded: {count}/{len(recorded)} cases")
    return 0


# A fixed, non-temporary path, cleared at the top of every run for the same reason ACTION_TMP is:
# `community.general.ini_file` is idempotent, and a file left over from a previous run would turn
# a recorded "section and option added" into "no change".
COLLECTION_TMP = "/tmp/volant16-golden-collection"


def collection_modules():
    """Record raw results from two collection modules that touch no host state at all: neither is
    served by an action plugin, so each runs as a plain Python module, the same way python_modules()
    does, over `connection: local`.

    `ansible.posix.sysctl` writes to `sysctl_file` alone (`sysctl_set` and `reload` both false):
    the form measured on a managed host with no kernel effect. `community.general.ini_file` is a
    module of a different collection with no system effect of its own, so the pair also proves the
    union carries more than one collection's modules without a name conflict.
    """
    modules = [
        {
            "name": "sysctl",
            "module": "ansible.posix.sysctl",
            "args": {
                "name": "net.ipv4.ip_forward",
                "value": "1",
                "sysctl_file": f"{COLLECTION_TMP}/sysctl.conf",
                "sysctl_set": False,
                "reload": False,
            },
        },
        {
            "name": "ini_file",
            "module": "community.general.ini_file",
            "args": {
                "path": f"{COLLECTION_TMP}/test.ini",
                "section": "golden",
                "option": "color",
                "value": "blue",
                # An explicit mode, not the umask-dependent default: ini_file reports it back, and
                # left to the umask it would name whoever ran the generator.
                "mode": "0644",
            },
        },
    ]
    tasks = [{"name": m["name"], m["module"]: m["args"]} for m in modules]
    play = [{"hosts": "localhost", "gather_facts": False, "connection": "local", "tasks": tasks}]
    env = dict(
        os.environ,
        ANSIBLE_STDOUT_CALLBACK="ansible.builtin.json",
        ANSIBLE_NOCOLOR="1",
        # Same reasoning as python_modules(): naming the interpreter outright skips discovery, so
        # neither its warning nor the fact it would set ever reaches these results.
        ANSIBLE_PYTHON_INTERPRETER="/usr/bin/python3",
    )
    shutil.rmtree(COLLECTION_TMP, ignore_errors=True)
    os.makedirs(COLLECTION_TMP, exist_ok=True)
    playbook = os.path.join(COLLECTION_TMP, "collection-modules.yml")
    with open(playbook, "w", encoding="utf-8") as f:
        yaml.safe_dump(play, f, default_flow_style=False, allow_unicode=True, sort_keys=False)
    run = subprocess.run(
        ["ansible-playbook", "-i", "localhost,", playbook],
        env=env,
        capture_output=True,
        text=True,
    )
    if run.returncode:
        print(
            f"ansible-playbook exited {run.returncode} while recording the collection modules",
            file=sys.stderr,
        )
        if run.stderr:
            print(run.stderr, file=sys.stderr)
        return 1
    report = json.loads(run.stdout)
    outcomes = {task["task"]["name"]: task["hosts"]["localhost"] for task in report["plays"][0]["tasks"]}
    destination = os.path.join(HERE, "collection")
    os.makedirs(destination, exist_ok=True)
    recorded = 0
    for m in modules:
        name = m["name"]
        result = dict(outcomes[name])
        result.pop("invocation", None)
        _redact_account(result, ("owner", "group"), ("uid", "gid"))
        with open(os.path.join(destination, f"{name}.json"), "w", encoding="utf-8") as f:
            json.dump(result, f, indent=2, ensure_ascii=False, sort_keys=True)
            f.write("\n")
        recorded += 1
    print(f"collection module goldens recorded: {recorded}/{len(modules)} modules")
    return 0


# A fixed path, cleared at the top of every run: most of these cases measure idempotence
# (`file-dir-same`, `lineinfile-same`), which anything left over from a previous pass would turn
# from "changed" into "no change". Under /var/tmp rather than /tmp: /tmp is often tmpfs, where
# `lsattr` fails (stat's `attributes` come back empty) and a directory's size counts its entries;
# /var/tmp sits on the root filesystem, ext4 on the development machine and on the CI runners.
NATIVE_TMP = "/var/tmp/volant-golden-native"
NATIVE_USER = "volantshape"
# A group of its own, not the user's name: `userdel` deletes a primary group named after the user,
# which would leave `group-removed` nothing to remove.
NATIVE_GROUP = "volantgrp"
STAT_VOLATILE = [f"stat.{key}" for key in ("atime", "mtime", "ctime", "inode", "dev", "version")]
# The `systemctl show` properties that move while a unit runs: times, process ids, the invocation,
# resource counters. Every other `status` value is compared.
LIVE_STATUS = re.compile(
    r"Timestamp|^(Main|Control|ExecMain)PID$|^InvocationID$|^ControlGroupId$|^CPUUsageNSec$"
    r"|(Current|Peak)$|^MemoryAvailable$|^IO(Read|Write)(Bytes|Operations)$"
    r"|^IP(Ingress|Egress)(Bytes|Packets)$|^NRestarts$"
)
# ExecStart and ExecStartEx hold one `{ path=... ; argv[]=... ; ... }` record per command, whose
# last fields describe the latest run. Those fields match a regex, the rest stays literal.
EXEC_STATUS = ("ExecStart", "ExecStartEx")
EXEC_LIVE_FIELD = re.compile(r"\b(start_time|stop_time)=\[[^\]]*\]|\bpid=-?\d+|\b(code|status)=\S+")
EXEC_FIELD_REGEX = {"start_time": r"\[[^\]]*\]", "stop_time": r"\[[^\]]*\]", "pid": r"-?\d+"}
# What the two cron cases keep of `status`. They are compared live, since the full `systemctl show`
# carries the host's systemd version (its key set), CPU set, memory size, task and file limits,
# process ids and times: the recording keeps the unit's identity, state and command, which name no
# machine.
STATUS_KEEP = (
    "Id", "Names", "Description", "LoadState", "ActiveState", "SubState", "UnitFileState",
    "FragmentPath", "Type", "ExecStart", "ExecStartEx", "After", "Before", "Requires", "WantedBy",
    "Conflicts",
)
# The `systemctl show` properties that list units, which systemd prints out of a hash set: the
# same unit gives `After=a b` on one query and `After=b a` on the next (measured). Compared as
# lists of words split on single spaces, sorted: the order is the only thing excused, a unit
# missing, extra or repeated still differs.
SET_STATUS = re.compile(
    r"^(Requires|Requisite|Wants|BindsTo|PartOf|Upholds|RequiredBy|RequisiteOf|WantedBy|BoundBy"
    r"|UpheldBy|ConsistsOf|Conflicts|ConflictedBy|Before|After|OnSuccess|OnSuccessOf|OnFailure"
    r"|OnFailureOf|Triggers|TriggeredBy|PropagatesReloadTo|ReloadPropagatedFrom|PropagatesStopTo"
    r"|StopPropagatedFrom|JoinsNamespaceOf|RequiresMountsFor|WantsMountsFor|Names|DropInPaths)$"
)
# What the reference appends to a file's name for its backup: `.<pid>.<YYYY-MM-DD@HH:MM:SS>~`.
BACKUP_SUFFIX = r"\.\d+\.\d{4}-\d{2}-\d{2}@\d{2}:\d{2}:\d{2}~$"
# What `package_facts` and `service_facts` keep of the machine's full inventory. Both are compared
# live, against a reference run next to the native one, so the recording only has to show the
# shape of an entry; the full lists would also name every package and unit of the machine that
# ran the generator.
LIVE_KEEP = {"packages": ("bash",), "services": ("cron.service", "systemd-journald.service")}

TMP_PLACEHOLDER = "<golden-tmp>"
HOST_PLACEHOLDER = "<golden-generator-host>"


def _literal(text):
    """`text` as a regex matching itself: only the metacharacters escaped, so the result reads the
    same in Python's `re` and in Rust's `regex`."""
    return re.sub(r"([\\.+*?()|\[\]{}^$])", r"\\\1", text)


def _exec_pattern(value):
    """An anchored regex for an ExecStart value: literal except the fields of the latest run."""
    parts, pos = [], 0
    for field in EXEC_LIVE_FIELD.finditer(value):
        key = field.group(0).split("=", 1)[0]
        parts += [_literal(value[pos:field.start()]), key, "=", EXEC_FIELD_REGEX.get(key, r"\S+")]
        pos = field.end()
    return "^" + "".join(parts) + _literal(value[pos:]) + "$"


def _at(value, path):
    for key in path.split("."):
        value = value.get(key) if isinstance(value, dict) else None
    return value


def _replace_in_strings(value, replacements):
    """NATIVE_TMP and the generating machine's name, wherever a string carries them: `user`'s
    invocation holds `ssh_key_comment: "ansible-generated on <hostname>"` even when no key is
    generated."""
    if isinstance(value, str):
        for old, new in replacements:
            value = value.replace(old, new)
        return value
    if isinstance(value, list):
        return [_replace_in_strings(v, replacements) for v in value]
    if isinstance(value, dict):
        return {k: _replace_in_strings(v, replacements) for k, v in value.items()}
    return value


def _mask_account(value, by_key):
    """Replace an ownership value only where it is the generating account's own: `root`, `0`, the
    synthetic account and anything else stay literal, so a native answering the wrong account, or
    `pw_name` where `gr_name` belongs, still differs once both sides are masked."""
    if isinstance(value, list):
        return [_mask_account(v, by_key) for v in value]
    if isinstance(value, dict):
        out = {}
        for key, v in value.items():
            real, placeholder = by_key.get(key, (None, None))
            mine = real is not None and not isinstance(v, bool) and v == real
            out[key] = placeholder if mine else _mask_account(v, by_key)
        return out
    return value


def _succeeds(*command):
    return subprocess.run(command, capture_output=True).returncode == 0


def natives():
    """Record the reference's answer for every case a native module of the agent must reproduce,
    and, in native/index.json, the path an enabled native has to take for each one.

    The cases run in one play, in the order below, each one meeting the state the previous ones
    left. `invocation` is kept: a native produces it too. NATIVE_TMP reads `<golden-tmp>`
    everywhere, the index's `args` included; the generating account reads `<user>`, `<group>`,
    `<uid>`, `<gid>` under the ownership keys, every other value is literal.

    An index entry says how a result is compared: `exact` key by key, `keys` the same key set with
    values compared too, `live` against a reference run made next to the native one, the recording
    giving only the shape. `volatile` lists the dotted paths whose value is not compared;
    `patterns` maps a path to the regex its value must match instead (a backup's name);
    `unordered` lists the paths whose value is a list of units in no fixed order (systemd's unit
    lists), compared as `sorted(value.split(" "))` on both sides. `branch`
    is what the reference's answer must show for the case to be the one its name claims; the
    generator fails when a recording disagrees.

    A `file`, `copy` or `lineinfile` case also records `_after`, read back once the task is done:
    whether its path exists, its type and mode, a regular file's content, and a backup's content.

    The cases with `become` need `sudo -n` and are left out without it; so are the two `cron`
    ones on a machine where cron is not both active and enabled, as `systemd-enabled-only` would
    otherwise enable it. A recording this run did not produce is deleted, so a stale one is never
    read as verified.
    """
    t = NATIVE_TMP
    sudo = _succeeds("sudo", "-n", "true")
    cron = sudo and _succeeds("systemctl", "is-active", "cron") and _succeeds("systemctl", "is-enabled", "cron")
    if not sudo:
        print("sudo -n true failed: the become cases are not recorded", file=sys.stderr)
    elif not cron:
        print("cron is not active and enabled: the two cron cases are not recorded", file=sys.stderr)
    to_placeholder = [(t, TMP_PLACEHOLDER), (socket.gethostname(), HOST_PLACEHOLDER)]

    tasks = []
    index = {}
    # Case name -> the result key naming its backup, or None, for every case recording `_after`.
    read_back = {}

    def case(name, module, args, branch, expect="native", why="", compare="exact", volatile=(), become=False):
        if become and not sudo:
            return
        task = {"name": name, module: args, "register": "last"}
        if become:
            task["become"] = True
        # Exactly the cases meant to fail: `ignore_errors` anywhere else would hide a case that
        # left its branch.
        if branch.get("failed"):
            task["ignore_errors"] = True
        tasks.append(task)
        patterns = {}
        if module in ("file", "copy", "lineinfile"):
            target = args.get("path") or args.get("dest")
            backup = ("backup_file" if module == "copy" else "backup") if args.get("backup") else None
            tasks.append({
                "name": f"after-stat-{name}",
                "stat": {"path": target, "get_checksum": False, "get_mime": False, "get_attributes": False},
                "register": "after",
            })
            tasks.append({"name": f"after-content-{name}", "slurp": {"src": target}, "when": "after.stat.isreg | default(false)"})
            if backup:
                tasks.append({"name": f"after-backup-{name}", "slurp": {"src": "{{ last.%s }}" % backup}})
                masked = _replace_in_strings(target, to_placeholder)
                patterns[backup] = "^" + _literal(masked) + BACKUP_SUFFIX
            read_back[name] = backup
        index[name] = {
            "module": module,
            "args": _replace_in_strings(args, to_placeholder),
            "become": become,
            "expect": expect,
            "why": why,
            "compare": compare,
            "volatile": list(volatile),
            "patterns": patterns,
            "unordered": [],
            "branch": branch,
        }

    def setup(module, args, become=False):
        if become and not sudo:
            return
        task = {"name": f"setup-{len(tasks)}", module: args}
        if become:
            task["become"] = True
        tasks.append(task)

    same = {"changed": False}
    changed = {"changed": True}

    def failed(msg):
        return {"changed": False, "failed": True, "msg": msg}

    for name, args in [
        ("stat-file", {"path": f"{t}/f.txt"}),
        ("stat-dir", {"path": t}),
        ("stat-link-follow", {"path": f"{t}/l", "follow": True}),
        ("stat-link-nofollow", {"path": f"{t}/l"}),
        ("stat-missing", {"path": f"{t}/nope"}),
        ("stat-plugin", {"path": f"{t}/f.txt", "follow": False, "get_checksum": True, "checksum_algorithm": "sha1"}),
        ("stat-bare", {"path": f"{t}/f.txt", "get_checksum": False, "get_mime": False, "get_attributes": False}),
    ]:
        case(name, "stat", args, same, volatile=() if name == "stat-missing" else STAT_VOLATILE)

    case("file-absent-missing", "file", {"path": f"{t}/gone", "state": "absent"}, same)
    setup("file", {"path": f"{t}/d0", "state": "directory"})
    case("file-absent-present", "file", {"path": f"{t}/d0", "state": "absent"}, changed)
    case("file-dir-created", "file", {"path": f"{t}/a/b", "state": "directory", "mode": "0755"}, changed)
    case("file-dir-same", "file", {"path": f"{t}/a/b", "state": "directory", "mode": 493}, same)
    case("file-dir-symbolic", "file", {"path": f"{t}/a/b", "state": "directory", "mode": "u=rwx,g=rx,o="}, changed)
    case("file-dest-alias", "file", {"dest": f"{t}/a/b", "state": "directory"}, same)
    case(
        "file-state-file-missing",
        "file",
        {"path": f"{t}/missing", "state": "file"},
        failed(f"file ({TMP_PLACEHOLDER}/missing) is absent, cannot continue"),
    )
    case("file-state-file-same", "file", {"path": f"{t}/f.txt", "state": "file"}, same)
    case("file-state-file-mode", "file", {"path": f"{t}/f.txt", "state": "file", "mode": "0600"}, changed)
    case(
        "file-dir-over-file",
        "file",
        {"path": f"{t}/f.txt", "state": "directory"},
        failed(f"{TMP_PLACEHOLDER}/f.txt already exists as a file"),
    )
    case(
        "file-owner-unknown",
        "file",
        {"path": f"{t}/f.txt", "owner": "volant-no-such-user"},
        failed("chown failed: failed to look up user volant-no-such-user"),
    )
    # Without become: the kernel refuses to give the generating account's file to root.
    case(
        "file-chown-denied",
        "file",
        {"path": f"{t}/f.txt", "owner": "root"},
        failed("chown failed"),
    )
    case("file-link", "file", {"path": f"{t}/l2", "src": f"{t}/f.txt", "state": "link"}, changed, "fallback", "state link")
    case(
        "file-recurse",
        "file",
        {"path": f"{t}/a", "state": "directory", "recurse": True, "mode": "0755"},
        changed,
        "fallback",
        "recurse",
    )

    # `content` reaches the module as a file the reference writes on the controller under a random
    # name, which the invocation quotes.
    staged = ["invocation.module_args._original_basename"]
    case("copy-module-created", "copy", {"content": "one\n", "dest": f"{t}/c.txt", "mode": "0644"}, changed, volatile=staged)
    case(
        "copy-module-modified-backup",
        "copy",
        {"content": "two\n", "dest": f"{t}/c.txt", "backup": True},
        changed,
        volatile=staged,
    )
    case(
        "copy-module-validate-fail",
        "copy",
        {"content": "", "dest": f"{t}/v.txt", "validate": "test -s %s"},
        failed("failed to validate"),
        volatile=staged,
    )
    case(
        "copy-module-no-dir",
        "copy",
        {"content": "x\n", "dest": f"{t}/nodir/x.txt"},
        failed(f"Destination directory {TMP_PLACEHOLDER}/nodir does not exist"),
        volatile=staged,
    )
    case(
        "copy-module-remote-src",
        "copy",
        {"src": f"{t}/f.txt", "dest": f"{t}/r.txt", "remote_src": True},
        changed,
        "fallback",
        "remote_src",
    )

    conf = f"{t}/l.conf"
    added = {"changed": True, "msg": "line added"}
    replaced = {"changed": True, "msg": "line replaced"}
    unchanged = {"changed": False, "msg": ""}
    case(
        "lineinfile-missing",
        "lineinfile",
        {"path": conf, "line": "a=1"},
        failed(f"Destination {TMP_PLACEHOLDER}/l.conf does not exist !"),
    )
    case("lineinfile-created", "lineinfile", {"path": conf, "line": "a=1", "create": True, "mode": "0644"}, added)
    case("lineinfile-same", "lineinfile", {"path": conf, "line": "a=1", "create": True, "mode": "0644"}, unchanged)
    case("lineinfile-appended", "lineinfile", {"path": conf, "regexp": "^b=", "line": "b=2"}, added)
    case(
        "lineinfile-replaced-backup",
        "lineinfile",
        {"path": conf, "regexp": "^a=", "line": "a=3", "backup": True},
        replaced,
    )
    case(
        "lineinfile-mode",
        "lineinfile",
        {"path": conf, "regexp": "^a=", "line": "a=3", "mode": "0600"},
        {"changed": True, "msg": "ownership, perms or SE linux context changed"},
    )
    case("lineinfile-insertafter", "lineinfile", {"path": conf, "line": "c=4", "insertafter": "^a="}, added)
    case("lineinfile-search-string", "lineinfile", {"path": conf, "search_string": "c=4", "line": "c=5"}, replaced)
    case("lineinfile-validate-ok", "lineinfile", {"path": conf, "line": "d=6", "validate": "test -s %s"}, added)
    case(
        "lineinfile-validate-fail",
        "lineinfile",
        {"path": conf, "line": "e=7", "validate": "false %s"},
        failed("failed to validate: rc:1 error:"),
    )
    case("lineinfile-absent", "lineinfile", {"path": conf, "regexp": "^d=", "state": "absent"}, {"changed": True, "msg": "1 line(s) removed"})
    case("lineinfile-absent-same", "lineinfile", {"path": conf, "regexp": "^d=", "state": "absent"}, unchanged)
    case("lineinfile-dir", "lineinfile", {"path": t, "line": "x"}, failed(f"Path {TMP_PLACEHOLDER} is a directory !"))
    case(
        "lineinfile-lookahead",
        "lineinfile",
        {"path": conf, "regexp": "^(?!#)a=", "line": "a=9"},
        replaced,
        "fallback",
        "regex syntax",
    )
    case(
        "lineinfile-backrefs",
        "lineinfile",
        {"path": conf, "regexp": "^(a)=.*", "line": "\\1=10", "backrefs": True},
        replaced,
        "fallback",
        "backrefs",
    )
    # Two matches of each expression, neither of them on the last line: the reference replaces the
    # last `^a=` and inserts after the last `^x=`, which a native acting on the first match, or
    # appending at the end, would not reproduce in `_after`.
    many = f"{t}/m.conf"
    setup("copy", {"content": "a=1\nx=1\na=2\nx=2\nz=0\n", "dest": many, "mode": "0644"})
    case("lineinfile-last-match", "lineinfile", {"path": many, "regexp": "^a=", "line": "a=3"}, replaced)
    case("lineinfile-insertafter-last", "lineinfile", {"path": many, "line": "y=1", "insertafter": "^x="}, added)

    live_status = (
        "status is the host's own systemctl show: its systemd version sets the key set, and CPU "
        "set, memory, limits, pids and times are the machine's; compared with a reference run on "
        "the same host"
    )
    if cron:
        case(
            "systemd-started-same",
            "systemd",
            {"name": "cron", "state": "started", "enabled": True},
            same,
            why=live_status,
            compare="live",
            become=True,
        )
        case(
            "systemd-enabled-only",
            "systemd_service",
            {"name": "cron.service", "enabled": True},
            same,
            why=live_status,
            compare="live",
            become=True,
        )
    case("systemd-daemon-reload", "systemd", {"daemon_reload": True}, same, become=True)
    case(
        "systemd-missing-unit",
        "systemd",
        {"name": "volant-no-such-unit", "state": "started"},
        failed("Could not find the requested service volant-no-such-unit: host"),
        become=True,
    )

    cache_time = ["cache_update_time"]
    fresh = {"changed": False, "cache_updated": False}
    case("apt-present-installed", "apt", {"name": "bash", "state": "present"}, fresh, volatile=cache_time, become=True)
    case("apt-present-list", "apt", {"name": ["bash", "coreutils"], "state": "present"}, fresh, volatile=cache_time, become=True)
    case("apt-absent-missing", "apt", {"name": "volant-no-such-package", "state": "absent"}, same, become=True)
    case(
        "apt-update-always",
        "apt",
        {"update_cache": True},
        {"cache_updated": True},
        "fallback",
        "update_cache without cache_valid_time",
        volatile=cache_time,
        become=True,
    )
    case("apt-update-fresh", "apt", {"update_cache": True, "cache_valid_time": 86400}, fresh, volatile=cache_time, become=True)
    case(
        "apt-present-update-fresh",
        "apt",
        {"name": "bash", "update_cache": True, "cache_valid_time": 86400},
        fresh,
        volatile=cache_time,
        become=True,
    )
    case(
        "apt-present-unknown",
        "apt",
        {"name": "volant-no-such-package", "state": "present"},
        failed("No package matching 'volant-no-such-package' is available"),
        "fallback",
        "not installed",
        become=True,
    )
    case("package-facts", "package_facts", {"manager": "auto"}, same, compare="live", become=True)
    case("service-facts", "service_facts", {}, same, compare="live", become=True)
    case("setup-pkg-mgr", "setup", {"gather_subset": ["!all"], "filter": ["ansible_pkg_mgr"]}, same)
    case("setup-service-mgr", "setup", {"gather_subset": ["!all"], "filter": ["ansible_service_mgr"]}, same)

    # The account and group are removed before the first case that creates them, in case an
    # interrupted run left them behind (`group-created` would record "no change"), and again in
    # `always`, which runs even when a case fails.
    removal = [
        {"name": f"cleanup-{module}", module: {"name": who, "state": "absent"}, "become": True}
        for module, who in (("user", NATIVE_USER), ("group", NATIVE_GROUP))
    ]
    if sudo:
        tasks.extend(dict(task, name=f"{task['name']}-before") for task in removal)
    account = {
        "name": NATIVE_USER,
        "uid": 64999,
        "group": NATIVE_GROUP,
        "shell": "/bin/sh",
        "create_home": False,
        "home": "/nonexistent-volantshape",
    }
    case("group-created", "group", {"name": NATIVE_GROUP, "gid": 64999}, changed, become=True)
    case("group-same", "group", {"name": NATIVE_GROUP, "gid": 64999}, same, become=True)
    case("user-created", "user", account, changed, become=True)
    case("user-same", "user", account, same, become=True)
    case("user-shell", "user", dict(account, shell="/bin/bash"), changed, become=True)
    # `!` is already what `useradd` left in shadow: no change, which the path check still catches.
    case("user-password", "user", {"name": NATIVE_USER, "password": "!"}, same, "fallback", "password", become=True)
    case("user-removed", "user", {"name": NATIVE_USER, "state": "absent"}, changed, become=True)
    case("user-absent-missing", "user", {"name": NATIVE_USER, "state": "absent"}, same, become=True)
    case("group-removed", "group", {"name": NATIVE_GROUP, "state": "absent"}, changed, become=True)

    block = {"block": tasks}
    if sudo:
        block["always"] = [dict(task, ignore_errors=True) for task in removal]
    play = [{"hosts": "localhost", "gather_facts": False, "connection": "local", "tasks": [block]}]
    env = dict(
        os.environ,
        ANSIBLE_STDOUT_CALLBACK="ansible.builtin.json",
        ANSIBLE_NOCOLOR="1",
        # Same reasoning as python_modules(): naming the interpreter outright skips discovery.
        ANSIBLE_PYTHON_INTERPRETER="/usr/bin/python3",
    )
    shutil.rmtree(t, ignore_errors=True)
    os.makedirs(t)
    # Explicit modes throughout, as in python_modules(): `stat-dir` reports the directory's, and a
    # file a case creates without a mode (`copy-module-remote-src`) gets one from the umask.
    os.chmod(t, 0o755)
    with open(f"{t}/f.txt", "w", encoding="utf-8") as f:
        f.write("hello\n")
    os.chmod(f"{t}/f.txt", 0o644)
    os.symlink("f.txt", f"{t}/l")
    umask = os.umask(0o022)
    try:
        # The playbook lives outside NATIVE_TMP: `stat-dir` reports that directory's link count,
        # which should depend on the fixture alone.
        with tempfile.TemporaryDirectory() as tmp:
            playbook = os.path.join(tmp, "natives.yml")
            # JSON, which is YAML: safe_dump would write an argument dict two cases share as an
            # anchor and an alias.
            with open(playbook, "w", encoding="utf-8") as f:
                json.dump(play, f, indent=1)
            run = subprocess.run(["ansible-playbook", "-i", "localhost,", playbook], env=env, capture_output=True, text=True)
    finally:
        os.umask(umask)
    if sudo:
        left = [f"{db} {who}" for db, who in (("passwd", NATIVE_USER), ("group", NATIVE_GROUP)) if _succeeds("getent", db, who)]
        if left:
            print(f"still on this machine: {', '.join(left)}; remove by hand", file=sys.stderr)
            return 1
    if run.returncode:
        print(f"ansible-playbook exited {run.returncode} while recording the native cases", file=sys.stderr)
        print(run.stdout[-4000:], run.stderr, sep="\n", file=sys.stderr)
        return 1

    report = json.loads(run.stdout)
    outcomes = {task["task"]["name"]: task["hosts"]["localhost"] for task in report["plays"][0]["tasks"]}
    me = pwd.getpwuid(os.getuid())
    by_key = {
        "owner": (me.pw_name, "<user>"),
        "pw_name": (me.pw_name, "<user>"),
        "group": (grp.getgrgid(os.getgid()).gr_name, "<group>"),
        "gr_name": (grp.getgrgid(os.getgid()).gr_name, "<group>"),
        "uid": (os.getuid(), "<uid>"),
        "gid": (os.getgid(), "<gid>"),
    }
    results = {}
    wrong = []
    for name, spec in index.items():
        result = _replace_in_strings(_redact_staged_paths(outcomes[name]), to_placeholder)
        result = _mask_account(result, by_key)
        status = result.get("status")
        if isinstance(status, dict) and status:
            # From the full `status`, before it is cut down: the live comparison meets every key.
            spec["volatile"] += [f"status.{key}" for key in sorted(status) if LIVE_STATUS.search(key)]
            spec["unordered"] = [f"status.{key}" for key in sorted(status) if SET_STATUS.search(key)]
            spec["patterns"].update(
                {f"status.{key}": _exec_pattern(status[key]) for key in EXEC_STATUS if key in status}
            )
            result["status"] = {key: value for key, value in status.items() if key in STATUS_KEEP}
        facts = result.get("ansible_facts", {})
        for key, keep in LIVE_KEEP.items():
            if isinstance(facts.get(key), dict):
                facts[key] = {k: v for k, v in facts[key].items() if k in keep}
        if name in read_back:
            stat_info = outcomes[f"after-stat-{name}"]["stat"]
            after = {"exists": stat_info["exists"]}
            if stat_info["exists"]:
                after["type"] = "directory" if stat_info["isdir"] else "link" if stat_info["islnk"] else "file"
                after["mode"] = stat_info["mode"]
            content = outcomes[f"after-content-{name}"]
            if not content.get("skipped"):
                after["content"] = base64.b64decode(content["content"]).decode()
            if read_back[name]:
                after["backup_content"] = base64.b64decode(outcomes[f"after-backup-{name}"]["content"]).decode()
            result["_after"] = after
        for key, want in dict({"failed": False}, **spec["branch"]).items():
            got = result.get(key, False if key == "failed" else None)
            if got != want:
                wrong.append(f"{name}: {key} is {got!r}, not {want!r}")
        for path, pattern in spec["patterns"].items():
            if not re.search(pattern, str(_at(result, path))):
                wrong.append(f"{name}: {path} {_at(result, path)!r} does not match {pattern}")
        results[name] = result
    if wrong:
        print("cases that did not land on the branch their name claims:", *wrong, sep="\n  ", file=sys.stderr)
        return 1

    destination = os.path.join(HERE, "native")
    os.makedirs(destination, exist_ok=True)
    for name, result in results.items():
        with open(os.path.join(destination, f"{name}.json"), "w", encoding="utf-8") as f:
            json.dump(result, f, indent=2, ensure_ascii=False, sort_keys=True)
            f.write("\n")
    with open(os.path.join(destination, "index.json"), "w", encoding="utf-8") as f:
        json.dump(index, f, indent=2, ensure_ascii=False, sort_keys=True)
        f.write("\n")
    for stale in sorted(set(os.listdir(destination)) - {f"{name}.json" for name in index} - {"index.json"}):
        os.remove(os.path.join(destination, stale))
        print(f"removed native/{stale}: this run did not record it", file=sys.stderr)
    print(f"native goldens recorded: {len(index)} cases")
    return 0


HOMONYM_WARNING = "Found both group and host with same name"


def inventory():
    inv = os.path.join(HERE, "inventory.ini")
    dump = json.loads(subprocess.run(["ansible-inventory", "-i", inv, "--list"], capture_output=True, text=True, check=True).stdout)
    hostvars = dump.get("_meta", {}).get("hostvars", {})
    patterns = {}
    with open(os.path.join(HERE, "patterns.txt"), encoding="utf-8") as f:
        for line in f:
            pattern = line.strip()
            if not pattern:
                continue
            run = subprocess.run(["ansible", "-i", inv, pattern, "--list-hosts"], capture_output=True, text=True)
            hosts = [h.strip() for h in run.stdout.splitlines()[1:] if h.strip()]
            # Specifically the homonym warning, not any "WARNING" substring: ansible also warns
            # on stderr for an unmatched term or an empty overall result ("Could not match
            # supplied host pattern", "No hosts matched"), which is a distinct concept our
            # Resolution keeps in `unmatched`, not `warnings`. Conflating the two here would make
            # this field warn for reasons resolve() was never asked to reproduce.
            patterns[pattern] = {"hosts": hosts, "warning": HOMONYM_WARNING in run.stderr}
    groups = {name: sorted(body.get("hosts", [])) for name, body in dump.items() if name != "_meta" and "hosts" in body}
    homonym = homonym_warning()
    with open(os.path.join(HERE, "expected_inventory.json"), "w", encoding="utf-8") as f:
        json.dump(
            {"hostvars": hostvars, "groups": groups, "patterns": patterns, "homonym": homonym},
            f,
            indent=2,
            ensure_ascii=False,
            sort_keys=True,
        )
        f.write("\n")
    print(f"inventory golden: {len(hostvars)} hosts, {len(patterns)} patterns")
    return 0


def listings():
    """What `--list-tasks`, `--list-tags`, `--list-hosts` and `--syntax-check` print, byte for
    byte, for every invocation in listing/args.txt.

    The fixtures in listing/ are written so this recording can be compared with anything: no
    play carries more than one tag and no play used with --list-hosts matches more than one
    host, because the reference prints both out of a Python set and a set of two short strings
    iterates in an order that changes from one process to the next (measured: four distinct
    orders in eight runs of the same command). A second tag or a second host would make this
    golden flap rather than fail, so `unreproducible` below refuses to write the recording at
    all when one appears, here on the machine that has the reference.

    Only stdout and the exit code are recorded. stderr carries warnings whose wording is a
    separate question from the layout this gate exists to pin.
    """
    here = os.path.join(HERE, "listing")
    # Every ANSIBLE_* variable goes, not a chosen few: an ansible.cfg above this directory, a
    # roles path, a tag, an inventory, any of them would change the answers. The recording has
    # to depend on the fixtures alone, and golden.rs clears the same prefix on its side.
    env = {k: v for k, v in os.environ.items() if not k.startswith("ANSIBLE_")}
    env["ANSIBLE_NOCOLOR"] = "1"
    expected = {}
    with open(os.path.join(here, "args.txt"), encoding="utf-8") as f:
        for line in f:
            line = line.strip()
            if not line or line.startswith("#"):
                continue
            run = subprocess.run(
                ["ansible-playbook", "-i", "inv.ini", *shlex.split(line)],
                cwd=here, env=env, capture_output=True, text=True,
            )
            bad = unreproducible(run.stdout)
            if bad:
                print(f"{line}: {bad}", file=sys.stderr)
                print(
                    "Refusing to write expected_listing.json: the reference prints a play's tags "
                    "and its hosts out of a Python set, so more than one of either is recorded in "
                    "an order that changes between processes. Reshape the fixture so the play "
                    "carries one tag and matches one host, or list it under a --limit that leaves "
                    "one.",
                    file=sys.stderr,
                )
                return 1
            expected[line] = {"stdout": run.stdout, "code": run.returncode}
    with open(os.path.join(HERE, "expected_listing.json"), "w", encoding="utf-8") as f:
        json.dump(expected, f, indent=2, ensure_ascii=False, sort_keys=True)
        f.write("\n")
    print(f"listing golden: {len(expected)} invocations recorded")
    return 0


PLAY_TAGS = re.compile(r"^  play #\d+ .*\tTAGS: \[(.*)\]$")
HOST_COUNT = re.compile(r"^    hosts \((\d+)\):$")


def unreproducible(stdout):
    """What in this output the reference would print in a different order next time, or None.

    Two things and only two: the play line's tags (`','.join(set(play.tags))`) and the host list
    under `--list-hosts` (`set(inventory.get_hosts(play.hosts))`). A task's own TAGS line is a
    sorted list on both sides and is left alone.
    """
    for line in stdout.splitlines():
        tags = PLAY_TAGS.match(line)
        if tags and "," in tags.group(1):
            return f"a play line carries more than one tag: {line.strip()}"
        hosts = HOST_COUNT.match(line)
        if hosts and int(hosts.group(1)) >= 2:
            return f"a play matches more than one host: {line.strip()}"
    return None


def homonym_warning():
    """A dedicated, minimal fixture where a group and one of its own hosts share a name: the one
    case in this golden that must warn, kept apart from inventory.ini so that fixture's patterns
    can test the opposite (that nothing there warns) without every one of them being drowned out
    by a single homonym anywhere in the inventory triggering Ansible's load-time warning."""
    inv = os.path.join(HERE, "homonym_inventory.ini")
    run = subprocess.run(["ansible", "-i", inv, "same", "--list-hosts"], capture_output=True, text=True)
    hosts = [h.strip() for h in run.stdout.splitlines()[1:] if h.strip()]
    return {"hosts": hosts, "warning": HOMONYM_WARNING in run.stderr}


if __name__ == "__main__":
    sys.exit(main())
