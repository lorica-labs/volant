# SPDX-License-Identifier: GPL-3.0-or-later
"""Runs every case in cases.yml through the reference ansible-playbook and records what it did.

The interpreter comes from the ansible-core tool environment, so PyYAML is available.
"""
import gzip
import io
import json
import os
import re
import shlex
import shutil
import subprocess
import sys
import tarfile
import tempfile

import yaml

HERE = os.path.dirname(os.path.abspath(__file__))
REFERENCE = open(os.path.join(HERE, "ANSIBLE_VERSION"), encoding="utf-8").read().strip()


def main() -> int:
    version = subprocess.run(["ansible-playbook", "--version"], capture_output=True, text=True, check=True).stdout
    if REFERENCE not in version.splitlines()[0]:
        print(f"ansible-playbook is not {REFERENCE}: {version.splitlines()[0]}", file=sys.stderr)
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
    # action_plugins() is now the newest and most environment-sensitive: `package` and `service`
    # need `become` and a real package manager or systemd. It goes last so a failure there cannot
    # also cost the three pre-existing, unrelated goldens.
    return inventory() or listings() or python_modules() or action_plugins()


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
# under the `src` key generate.py's brief keys off.
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
    setup("file", {"path": dest_dir, "state": "directory"})
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
    setup("file", {"path": unpacked_dir, "state": "directory"})
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
        with open(os.path.join(destination, f"{name}.json"), "w", encoding="utf-8") as f:
            json.dump(result, f, indent=2, ensure_ascii=False, sort_keys=True)
            f.write("\n")
        count += 1
    print(f"action plugin goldens recorded: {count}/{len(recorded)} cases")
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
