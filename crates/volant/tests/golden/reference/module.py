# SPDX-License-Identifier: GPL-3.0-or-later
"""The `module` kind: one play of module cases on localhost, recorded in `module/`.

The cases run in one play, in the order below, each one meeting the state the previous ones left.
`module/<case>.json` is a case's result, `module/index.json` says how each is compared and which
path an enabled native has to take, and `module/play.json` is the play that ran.

An index entry's `compare` is `exact` (key by key, `invocation` kept: a native produces it too),
`live` (against a reference run made next to the native one, the recording giving only the shape)
or `shape` (the key set and the type of each value, `invocation` dropped: the modules with no
native, compared by golden.rs's python module, action plugin and collection tests). `volatile`
lists the dotted paths whose value is not compared; `patterns` maps a path to the regex its value
must match instead (a backup's name); `unordered` lists the paths whose value is a list of units in
no fixed order, compared as `sorted(value.split(" "))` on both sides. `branch` is what the
reference's answer must show for the case to be the one its name claims; the generator fails when
a recording disagrees.

NATIVE_TMP reads `<golden-tmp>` everywhere, the index's `args` and the play included, and ACTION_SRC
`<golden-action-src>`; an `exact` or `live` recording masks the generating account as `<user>`,
`<group>`, `<uid>`, `<gid>` under the ownership keys, a `shape` one as SHAPE_ACCOUNT. A `file`,
`copy` or `lineinfile` case compared `exact` also records `_after`, read back once the task is done:
whether its path exists, its type and mode, a regular file's content, and a backup's content.

`systemd` and `apt` are also asked for changes, on NATIVE_UNIT, a unit the play installs, and on
NATIVE_PACKAGE, a package it installs and removes; both are removed in `always`. The play runs
under the native lock.

The cases with `become` need `sudo -n` and are left out without it; so are the two `cron` ones on a
machine where cron is not both active and enabled, as `systemd-enabled-only` would otherwise enable
it. A recording this run did not produce is deleted, so a stale one is never read as verified.
"""
import base64
import gzip
import io
import json
import os
import re
import shutil
import socket
import subprocess
import sys
import tarfile

from . import GOLDEN
from . import redact
from .run import Failed, json_report, native_lock, outcomes, run, scratch, write_json

DESTINATION = os.path.join(GOLDEN, "module")
# The inputs of the action plugin cases, committed.
ACTION_SRC = os.path.join(GOLDEN, "action-src")
ACTION_SRC_PLACEHOLDER = "<golden-action-src>"
# Fixed, non-temporary paths, emptied by the play before its cases: `copy-same`, `template-same` and
# `lineinfile`'s "line added" measure what the directory already holds.
PYTHON_TMP = "/tmp/volant15-golden"
ACTION_TMP = "/tmp/volant16-golden"
COLLECTION_TMP = "/tmp/volant16-golden-collection"
# Written by the play with a fixed content and mode, so `stat`'s checksum and mode name nothing.
STAT_TARGET = os.path.join(PYTHON_TMP, "golden-stat-target")
STAT_TARGET_CONTENT = "golden stat fixture\n"
SHAPE_WHY = "no native: compared by its keys and the type of each value"


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
    """The three inputs of the action plugin cases, written once: once committed, a fixed content
    is the point, so a fixture already on disk is left untouched rather than regenerated."""
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
# The account's home, which `user-created-home` creates (`useradd -m`); removed before and after
# all the same.
NATIVE_HOME = "/nonexistent-volantshape"
# Where `user-home-moved` points the account, which nothing creates.
NATIVE_HOME_MOVED = "/nonexistent-volantshape-moved"
STAT_VOLATILE = [f"stat.{key}" for key in ("atime", "mtime", "ctime", "inode", "dev", "version")]
# The `systemctl show` properties that move while a unit runs: times, process ids, the invocation,
# resource counters. Every other `status` value is compared.
LIVE_STATUS = re.compile(
    r"Timestamp|^(Main|Control|ExecMain)PID$|^InvocationID$|^ControlGroupId$|^CPUUsageNSec$"
    r"|(Current|Peak)$|^MemoryAvailable$|^IO(Read|Write)(Bytes|Operations)$"
    r"|^IP(Ingress|Egress)(Bytes|Packets)$|^NRestarts$"
)
# ExecStart, ExecReload and the other `Exec*` properties hold one `{ path=... ; argv[]=... ; ... }`
# record per command, whose last fields describe the latest run: the reload's too, once the unit
# was reloaded. Those fields match a regex, the rest stays literal.
EXEC_STATUS = re.compile(r"^Exec")
EXEC_RECORD = "{ path="
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
# ran the generator. One unit per `service_facts` branch: `list-units` (`cron.service`, whose
# status `list-unit-files` replaces, and `systemd-journald.service`), the SysV listing (`cron`) and
# the template units only `list-unit-files` names (`LIVE_KEEP_SUFFIX`). golden.rs keeps the same.
LIVE_KEEP = {
    "packages": ("bash",),
    "services": ("cron.service", "systemd-journald.service", "cron"),
}
LIVE_KEEP_SUFFIX = {"services": "@.service"}

# A unit of the generator's own, so `systemd` can be asked to change something: started from
# stopped, stopped, restarted, reloaded, enabled and disabled from the opposite state.
NATIVE_UNIT = "volant-golden"
NATIVE_UNIT_FILE = f"/etc/systemd/system/{NATIVE_UNIT}.service"
NATIVE_UNIT_TEXT = (
    "[Unit]\nDescription=Volant golden throwaway unit\n\n"
    "[Service]\nExecStart=/bin/sleep infinity\nExecReload=/bin/true\n\n"
    "[Install]\nWantedBy=multi-user.target\n"
)
NATIVE_UNIT_TEXT_EDITED = NATIVE_UNIT_TEXT.replace("throwaway unit", "throwaway unit, edited")
# What each unit case leaves, read back after it into `_after`.
UNIT_READ_BACK = f"systemctl show {NATIVE_UNIT} -p ActiveState -p UnitFileState -p NeedDaemonReload -p Description"
# A small package in Debian and Ubuntu's main archive, installed and removed by the play: the one
# `apt` change the cases ask for. A machine that already has it is refused rather than stripped.
NATIVE_PACKAGE = "hello"
# What apt and dpkg print while changing a package: download sizes, the mirror, the count of files
# already installed. Replaced by one placeholder in the recording and not compared.
MACHINE_OUTPUT = ["stdout", "stdout_lines", "stderr", "stderr_lines"]


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


def _succeeds(*command):
    return subprocess.run(command, capture_output=True).returncode == 0


def _installed(package):
    """Whether dpkg has `package` installed, config files alone not counting."""
    query = subprocess.run(["dpkg-query", "-W", "-f=${db:Status-Status}", package], capture_output=True, text=True)
    return query.stdout == "installed"


def record():
    with native_lock():
        _record()


def _record():
    t = NATIVE_TMP
    sudo = _succeeds("sudo", "-n", "true")
    cron = sudo and _succeeds("systemctl", "is-active", "cron") and _succeeds("systemctl", "is-enabled", "cron")
    if not sudo:
        print("sudo -n true failed: the become cases are not recorded", file=sys.stderr)
    elif not cron:
        print("cron is not active and enabled: the two cron cases are not recorded", file=sys.stderr)
    # ACTION_SRC first: the machine's name could be part of the checkout's path.
    to_placeholder = [
        (ACTION_SRC, ACTION_SRC_PLACEHOLDER),
        (t, redact.TMP),
        (socket.gethostname(), redact.HOST),
    ]

    tasks = []
    index = {}
    # Case name -> the result key naming its backup, or None, for every case recording `_after`.
    read_back = {}

    # Case name -> the MACHINE_OUTPUT keys its recording replaces with redact.MACHINE_OUTPUT.
    machine = {}
    # The unit cases followed by an `after-unit-` read-back.
    unit_back = set()
    # The cases whose recording keeps the facts' names and none of their values.
    facts_shape = set()

    def case(
        name,
        module,
        args,
        branch,
        expect="native",
        why="",
        compare="exact",
        volatile=(),
        become=False,
        output=False,
        keywords=None,
    ):
        if become and not sudo:
            return
        if output:
            machine[name] = MACHINE_OUTPUT
            volatile = list(volatile) + MACHINE_OUTPUT
        task = {"name": name, module: args, "register": "last"}
        if become:
            task["become"] = True
        # Exactly the cases meant to fail: `ignore_errors` anywhere else would hide a case that
        # left its branch.
        if branch.get("failed"):
            task["ignore_errors"] = True
        task.update(keywords or {})
        tasks.append(task)
        patterns = {}
        if module in ("file", "copy", "lineinfile") and compare != "shape":
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
                masked = redact.in_strings(target, to_placeholder)
                patterns[backup] = "^" + _literal(masked) + BACKUP_SUFFIX
            read_back[name] = backup
        if module == "systemd" and args.get("name") == NATIVE_UNIT:
            # The unit as the case left it: `status` is its state before the action.
            tasks.append({"name": f"after-unit-{name}", "command": UNIT_READ_BACK})
            unit_back.add(name)
        index[name] = {
            "module": module,
            "args": redact.in_strings(args, to_placeholder),
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
        failed(f"file ({redact.TMP}/missing) is absent, cannot continue"),
    )
    case("file-state-file-same", "file", {"path": f"{t}/f.txt", "state": "file"}, same)
    case("file-state-file-mode", "file", {"path": f"{t}/f.txt", "state": "file", "mode": "0600"}, changed)
    case(
        "file-dir-over-file",
        "file",
        {"path": f"{t}/f.txt", "state": "directory"},
        failed(f"{redact.TMP}/f.txt already exists as a file"),
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
        failed(f"Destination directory {redact.TMP}/nodir does not exist"),
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
        failed(f"Destination {redact.TMP}/l.conf does not exist !"),
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
    case("lineinfile-dir", "lineinfile", {"path": t, "line": "x"}, failed(f"Path {redact.TMP} is a directory !"))
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
    # The generator's own unit, from nothing to running and back: each case asks for a change.
    # Its status is compared live for the same reason as cron's. Removed first, in case an
    # interrupted run left it running, and again in `always`.
    unit_cleanup = {
        "name": "cleanup-unit",
        "shell": f"systemctl disable --now {NATIVE_UNIT}.service; rm -f {NATIVE_UNIT_FILE}; systemctl daemon-reload",
        "become": True,
    }
    if sudo:
        tasks.append(dict(unit_cleanup, name="cleanup-unit-before"))
    setup("copy", {"content": NATIVE_UNIT_TEXT, "dest": NATIVE_UNIT_FILE, "mode": "0644"}, become=True)
    unit_live = {"why": live_status, "compare": "live", "become": True}

    def unit_case(name, args, branch):
        case(name, "systemd", dict(args, name=NATIVE_UNIT), branch, **unit_live)

    def probe(var, prop):
        tasks.append({"name": f"probe-{var}", "command": f"systemctl show {NATIVE_UNIT} -p {prop} --value", "register": var})

    def changed_since(before, after, what):
        # Without `ignore_errors`: an action that did nothing fails the play, on either side.
        tasks.append({
            "name": f"assert-{after}",
            "assert": {"that": [f"{after}.stdout != {before}.stdout"], "fail_msg": f"{what} left the unit as it was"},
        })

    unit_case("systemd-unit-started", {"state": "started"}, changed)
    # On the running unit, where only a new main process tells a restart from a start.
    probe("pid_before", "MainPID")
    unit_case("systemd-unit-restarted", {"state": "restarted"}, changed)
    probe("pid_after", "MainPID")
    changed_since("pid_before", "pid_after", "restarted")
    probe("reload_before", "ExecReload")
    unit_case("systemd-unit-reloaded", {"state": "reloaded"}, changed)
    probe("reload_after", "ExecReload")
    changed_since("reload_before", "reload_after", "reloaded")
    # The unit file edited under systemd: only a daemon reload makes the new Description show and
    # NeedDaemonReload go back to `no`.
    setup("copy", {"content": NATIVE_UNIT_TEXT_EDITED, "dest": NATIVE_UNIT_FILE, "mode": "0644"}, become=True)
    unit_case("systemd-unit-daemon-reload", {"daemon_reload": True}, same)
    unit_case("systemd-unit-stopped", {"state": "stopped"}, changed)
    unit_case("systemd-unit-enabled", {"enabled": True}, changed)
    unit_case("systemd-unit-disabled", {"enabled": False}, changed)
    # So that what `disabled` did is seen by a case of its own.
    unit_case("systemd-unit-disabled-same", {"enabled": False}, same)

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
    # A package the machine does not have: installed, found installed, removed.
    case(
        "apt-install-absent",
        "apt",
        {"name": NATIVE_PACKAGE, "state": "present"},
        {"changed": True, "cache_updated": False},
        "fallback",
        "not installed",
        volatile=cache_time,
        become=True,
        output=True,
    )
    case("apt-present-installed-now", "apt", {"name": NATIVE_PACKAGE, "state": "present"}, fresh, volatile=cache_time, become=True)
    case(
        "apt-remove-installed",
        "apt",
        {"name": NATIVE_PACKAGE, "state": "absent"},
        changed,
        "fallback",
        "installed",
        become=True,
        output=True,
    )
    case("package-facts", "package_facts", {"manager": "auto"}, same, compare="live", become=True)
    case("service-facts", "service_facts", {}, same, compare="live", become=True)
    case("setup-pkg-mgr", "setup", {"gather_subset": ["!all"], "filter": ["ansible_pkg_mgr"]}, same)
    case("setup-service-mgr", "setup", {"gather_subset": ["!all"], "filter": ["ansible_service_mgr"]}, same)
    # The `min` facts, which the native answers. They are the machine's (its name, id, addresses,
    # keys, kernel command line), so compared live; the recording keeps their names only. The
    # environment is the module's process's own, which is not Volant's agent's on one side and
    # ansible-playbook's on the other, and the date and time move.
    case(
        "setup-min",
        "setup",
        {"gather_subset": ["min"]},
        same,
        why="the facts are the machine's: its name, id, addresses and keys; compared with a reference run on the same host",
        compare="live",
        volatile=["ansible_facts.ansible_env", "ansible_facts.ansible_date_time"],
    )
    facts_shape.add("setup-min")

    # Two more `copy` cases, last so that no task name before them moves: a `checksum` the staged
    # file does not have, and the fixture's link `l` as `dest`, which is replaced by a file of its
    # own while `f.txt` keeps its content. The `stat-link-*` cases are the only others to read `l`.
    case(
        "copy-module-checksum",
        "copy",
        {"content": "k\n", "dest": f"{t}/k.txt", "checksum": "0000"},
        failed("Copied file does not match the expected checksum. Transfer failed."),
        volatile=staged,
    )
    case("copy-module-dest-link", "copy", {"content": "one\n", "dest": f"{t}/l"}, changed, volatile=staged)

    # The account and group are removed before the first case that creates them, in case an
    # interrupted run left them behind (`group-created` would record "no change"), and again in
    # `always`, which runs even when a case fails.
    # Accounts first: `groupdel` refuses a group that is still an account's primary one. The
    # account `user-same-name-group` names after the group, and the user group
    # `user-created-home` gets, go too.
    removal = [
        {"name": f"cleanup-{name}", module: {"name": who, "state": "absent"}, "become": True}
        for name, module, who in (
            ("user", "user", NATIVE_USER),
            ("user-named-group", "user", NATIVE_GROUP),
            ("group", "group", NATIVE_GROUP),
            ("user-group", "group", NATIVE_USER),
        )
    ] + [
        {"name": f"cleanup-{name}", "file": {"path": path, "state": "absent"}, "become": True}
        for name, path in (("home", NATIVE_HOME), ("home-moved", NATIVE_HOME_MOVED))
    ]
    if sudo:
        tasks.extend(dict(task, name=f"{task['name']}-before") for task in removal)
    account = {
        "name": NATIVE_USER,
        "uid": 64999,
        "group": NATIVE_GROUP,
        "shell": "/bin/sh",
        "create_home": False,
        "home": NATIVE_HOME,
    }
    case("group-created", "group", {"name": NATIVE_GROUP, "gid": 64999}, changed, become=True)
    case("group-same", "group", {"name": NATIVE_GROUP, "gid": 64999}, same, become=True)
    case("user-created", "user", account, changed, become=True)
    case("user-same", "user", account, same, become=True)
    case("user-shell", "user", dict(account, shell="/bin/bash"), changed, become=True)
    # `!` is already what `useradd` left in shadow: no change, which the path check still catches.
    # `create_home: false` as in `account`: the module's default would create the missing home
    # and answer `changed` for that, which a machine keeping the home of an earlier run would not.
    case(
        "user-password",
        "user",
        {"name": NATIVE_USER, "password": "!", "create_home": False},
        same,
        "fallback",
        "password",
        become=True,
    )
    case("user-removed", "user", {"name": NATIVE_USER, "state": "absent"}, changed, become=True)
    case("user-absent-missing", "user", {"name": NATIVE_USER, "state": "absent"}, same, become=True)
    # The account again, from the module's defaults: `create_home` left true makes `useradd -m`
    # create NATIVE_HOME from the skeleton, and no `group` gives it a user group of its own name,
    # a second group for the membership cases. `groups` and `comment` at creation too.
    home_account = {
        "name": NATIVE_USER,
        "uid": 64998,
        "home": NATIVE_HOME,
        "groups": [NATIVE_GROUP],
        "comment": "Volant shape",
    }
    # The user group's gid is the next free one on the machine that records.
    case("user-created-home", "user", home_account, changed, volatile=["group"], become=True)
    case("user-home-same", "user", home_account, same, volatile=["group"], become=True)
    # A uid, a primary group given by name (usermod takes its gid) and a comment, at once.
    case(
        "user-comment-uid-group",
        "user",
        {"name": NATIVE_USER, "uid": 64997, "group": NATIVE_GROUP, "comment": "Volant shape 2"},
        changed,
        become=True,
    )
    # The memberships, from volantgrp alone: `-a -G` adds one; `append` where only a removal
    # differs runs nothing; without `append` the missing one goes; `[]` clears; a string is split
    # on commas; `""` clears too.
    member = {"name": NATIVE_USER}
    case("user-groups-append", "user", dict(member, groups=[NATIVE_USER], append=True), changed, become=True)
    case("user-groups-append-same", "user", dict(member, groups=[NATIVE_GROUP], append=True), same, become=True)
    case("user-groups-remove", "user", dict(member, groups=[NATIVE_USER]), changed, become=True)
    case("user-groups-cleared", "user", dict(member, groups=[]), changed, become=True)
    case("user-groups-string", "user", dict(member, groups=f"{NATIVE_GROUP},{NATIVE_USER}"), changed, become=True)
    case("user-groups-empty-string", "user", dict(member, groups=""), changed, become=True)
    case("user-groups-cleared-same", "user", dict(member, groups=[]), same, become=True)
    # `usermod -d` alone: `create_home: false`, so nothing creates the new home.
    case("user-home-moved", "user", dict(member, home=NATIVE_HOME_MOVED, create_home=False), changed, become=True)
    # Its own group as primary again, so `userdel` takes that group with the account.
    case("user-group-back", "user", dict(member, group=NATIVE_USER, create_home=False), changed, volatile=["group"], become=True)
    case("user-removed-home", "user", {"name": NATIVE_USER, "state": "absent"}, changed, become=True)
    # An account named after an existing group: `useradd -N`, no user group; and `-r`. No `uid`:
    # a system account given one above SYS_UID_MAX makes some `useradd` versions warn (4.13 does,
    # 4.17 does not), and the warning is the module's `stderr`. The one `useradd` picks is the
    # machine's.
    case(
        "user-same-name-group",
        "user",
        {"name": NATIVE_GROUP, "system": True, "create_home": False, "home": NATIVE_HOME},
        changed,
        volatile=["uid"],
        become=True,
    )
    case("user-same-name-group-removed", "user", {"name": NATIVE_GROUP, "state": "absent"}, changed, become=True)
    case("group-gid-changed", "group", {"name": NATIVE_GROUP, "gid": 64995}, changed, become=True)
    case("group-removed", "group", {"name": NATIVE_GROUP, "state": "absent"}, changed, become=True)
    system_group = {"name": NATIVE_GROUP, "gid": 64999, "system": True}
    case("group-system-created", "group", system_group, changed, become=True)
    case("group-system-same", "group", system_group, same, become=True)
    # The commands' own refusals, which the module answers with their error output: a gid
    # another group holds, a uid another account holds. Neither leaves anything behind.
    case("group-gid-taken", "group", {"name": NATIVE_USER, "gid": 64999}, {"failed": True}, become=True)
    case(
        "user-uid-taken",
        "user",
        {"name": NATIVE_USER, "uid": 0, "group": NATIVE_GROUP, "create_home": False},
        {"failed": True},
        become=True,
    )
    case("group-system-removed", "group", {"name": NATIVE_GROUP, "state": "absent"}, changed, become=True)

    # The modules no native answers, recorded for their keys and the type of each value. Last, so
    # that no task name before them moves, and in directories of their own, which the play empties
    # first.
    def shape(name, module, args, branch, keywords=None, become=False):
        case(name, module, args, branch, "python", SHAPE_WHY, "shape", become=become, keywords=keywords)

    for directory in (PYTHON_TMP, ACTION_TMP, COLLECTION_TMP):
        setup("file", {"path": directory, "state": "absent"})
        setup("file", {"path": directory, "state": "directory", "mode": "0755"})
    setup("copy", {"content": STAT_TARGET_CONTENT, "dest": STAT_TARGET, "mode": "0644"})
    shape("python-ping", "ping", {}, same)
    # STAT_TARGET, not a file of the machine: /etc/hostname's checksum would be a confirmable hash
    # of the machine's name.
    shape("python-stat", "stat", {"path": STAT_TARGET}, same)
    shape("python-file", "file", {"path": f"{PYTHON_TMP}/golden-file", "state": "touch", "mode": "0644"}, changed)
    shape("python-lineinfile", "lineinfile", {"path": f"{PYTHON_TMP}/golden-line", "line": "hello", "create": True}, added)

    hello, motd, bundle = _ensure_action_fixtures()
    dest_dir = f"{ACTION_TMP}/dir"
    unpacked_dir = f"{ACTION_TMP}/unpacked"
    new = {"src": hello, "dest": f"{ACTION_TMP}/new.txt", "mode": "0644"}
    shape("action-copy-new", "copy", new, changed)
    shape("action-copy-same", "copy", new, same)
    shape("action-copy-content", "copy", {"content": "x\n", "dest": f"{ACTION_TMP}/content.txt", "mode": "0644"}, changed)
    shape("action-copy-force-false", "copy", {"content": "y\n", "dest": f"{ACTION_TMP}/content.txt", "force": False}, same)
    # Explicit modes on both directories: `unarchive-local` reports its destination's.
    setup("file", {"path": dest_dir, "state": "directory", "mode": "0775"})
    shape("action-copy-dest-dir", "copy", {"src": hello, "dest": f"{dest_dir}/", "mode": "0644"}, changed)
    shape(
        "action-copy-validate-fail",
        "copy",
        {"content": "", "dest": f"{ACTION_TMP}/invalid.txt", "validate": "test -s %s"},
        failed("failed to validate"),
    )
    shape(
        "action-copy-remote-src",
        "copy",
        {"src": f"{ACTION_TMP}/new.txt", "dest": f"{ACTION_TMP}/remote.txt", "remote_src": True, "mode": "0644"},
        changed,
    )
    template_vars = {"vars": {"who": "golden", "extra": True}}
    motd_new = {"src": motd, "dest": f"{ACTION_TMP}/motd", "mode": "0644"}
    shape("action-template-new", "template", motd_new, changed, template_vars)
    shape("action-template-same", "template", motd_new, same, template_vars)
    shape(
        "action-template-lstrip",
        "template",
        {"src": motd, "dest": f"{ACTION_TMP}/motd-lstrip", "mode": "0644", "lstrip_blocks": True},
        changed,
        template_vars,
    )
    shape("action-package-present", "package", {"name": "bash", "state": "present"}, same, become=True)
    shape("action-service-started", "service", {"name": "systemd-journald", "state": "started"}, same, become=True)
    setup("file", {"path": unpacked_dir, "state": "directory", "mode": "0775"})
    shape("action-unarchive-local", "unarchive", {"src": bundle, "dest": unpacked_dir}, changed)
    shape(
        "action-unarchive-creates",
        "unarchive",
        {"src": bundle, "dest": unpacked_dir, "creates": f"{unpacked_dir}/inside.txt"},
        same,
    )

    # `sysctl_file` alone, with `sysctl_set` and `reload` false: no kernel effect. `ini_file` is a
    # module of another collection, so the pair also shows the union carrying two collections.
    shape(
        "collection-sysctl",
        "ansible.posix.sysctl",
        {
            "name": "net.ipv4.ip_forward",
            "value": "1",
            "sysctl_file": f"{COLLECTION_TMP}/sysctl.conf",
            "sysctl_set": False,
            "reload": False,
        },
        changed,
    )
    shape(
        "collection-ini-file",
        "community.general.ini_file",
        {"path": f"{COLLECTION_TMP}/test.ini", "section": "golden", "option": "color", "value": "blue", "mode": "0644"},
        {"changed": True, "msg": "section and option added"},
    )

    block = {"block": tasks}
    if sudo:
        package_cleanup = {"name": "cleanup-package", "apt": {"name": NATIVE_PACKAGE, "state": "absent"}, "become": True}
        block["always"] = [dict(task, ignore_errors=True) for task in removal + [unit_cleanup, package_cleanup]]
    # The interpreter named in the play, as a playbook pins it, so the recording, the replay under
    # Volant and the reference's own replay all run the same one: left to discovery, both engines
    # take the first of python3.13, python3.12, ... that the host has, which on a runner with
    # python3.12 is `/usr/bin/python3.12` and shows in `ansible_python.executable`.
    play = [{
        "hosts": "localhost",
        "gather_facts": False,
        "connection": "local",
        "vars": {"ansible_python_interpreter": "/usr/bin/python3"},
        "tasks": [block],
    }]
    if sudo and _installed(NATIVE_PACKAGE):
        raise Failed(
            f"{NATIVE_PACKAGE} is installed on this machine, and the play installs and removes it: "
            "remove it by hand if an interrupted run left it, or record elsewhere"
        )
    shutil.rmtree(t, ignore_errors=True)
    os.makedirs(t)
    # Explicit modes throughout: `stat-dir` reports the directory's, and a file a case creates
    # without a mode (`copy-module-remote-src`) gets one from the umask.
    os.chmod(t, 0o755)
    with open(f"{t}/f.txt", "w", encoding="utf-8") as f:
        f.write("hello\n")
    os.chmod(f"{t}/f.txt", 0o644)
    os.symlink("f.txt", f"{t}/l")
    umask = os.umask(0o022)
    try:
        # The playbook lives outside NATIVE_TMP: `stat-dir` reports that directory's link count,
        # which should depend on the fixture alone.
        with scratch() as tmp:
            playbook = os.path.join(tmp, "natives.yml")
            # JSON, which is YAML: safe_dump would write an argument dict two cases share as an
            # anchor and an alias.
            with open(playbook, "w", encoding="utf-8") as f:
                json.dump(play, f, indent=1)
            # callback_plugins/golden_json.py, which golden.rs also loads: `ansible.builtin.json`
            # is `ansible.posix`'s in ansible-core 2.19, and the replay must not need it.
            result = run(["ansible-playbook", "-i", "localhost,", playbook], extra_env={"ANSIBLE_STDOUT_CALLBACK": "golden_json"})
    finally:
        os.umask(umask)
    if sudo:
        accounts = [("passwd", NATIVE_USER), ("passwd", NATIVE_GROUP), ("group", NATIVE_GROUP), ("group", NATIVE_USER)]
        left = [f"{db} {who}" for db, who in accounts if _succeeds("getent", db, who)]
        left += [NATIVE_UNIT_FILE] * os.path.exists(NATIVE_UNIT_FILE) + [NATIVE_PACKAGE] * _installed(NATIVE_PACKAGE)
        left += [home for home in (NATIVE_HOME, NATIVE_HOME_MOVED) if os.path.exists(home)]
        if left:
            raise Failed(f"still on this machine: {', '.join(left)}; remove by hand")
    found = outcomes(json_report(result, "the module cases"))

    results = {}
    wrong = []
    for name, spec in index.items():
        if spec["compare"] == "shape":
            result = dict(found[name])
            result.pop("invocation", None)
            result = redact.systemd_values(redact.staged_paths(redact.accounts(result, redact.SHAPE_ACCOUNT)))
            uncut = result
        else:
            result = redact.in_strings(redact.staged_paths(found[name]), to_placeholder)
            result = redact.accounts(result, redact.NATIVE_ACCOUNT)
            for key in machine.get(name, ()):
                if key in result:
                    result[key] = redact.MACHINE_OUTPUT
            uncut = result
            status = result.get("status")
            if isinstance(status, dict) and status:
                # From the full `status`, before it is cut down: the live comparison meets every key.
                spec["volatile"] += [f"status.{key}" for key in sorted(status) if LIVE_STATUS.search(key)]
                spec["unordered"] = [f"status.{key}" for key in sorted(status) if SET_STATUS.search(key)]
                spec["patterns"].update(
                    {
                        f"status.{key}": _exec_pattern(value)
                        for key, value in status.items()
                        if EXEC_STATUS.search(key) and str(value).startswith(EXEC_RECORD)
                    }
                )
                # The patterns are checked against the whole status, which the live comparison meets.
                uncut = dict(result)
                result["status"] = {key: value for key, value in status.items() if key in STATUS_KEEP}
            facts = result.get("ansible_facts", {})
            if name in facts_shape:
                result["ansible_facts"] = facts = {key: redact.MACHINE_OUTPUT for key in facts}
            for key, keep in LIVE_KEEP.items():
                if isinstance(facts.get(key), dict):
                    suffix = LIVE_KEEP_SUFFIX.get(key)
                    facts[key] = {
                        k: v
                        for k, v in facts[key].items()
                        if k in keep or (suffix is not None and k.endswith(suffix))
                    }
            if name in read_back:
                stat_info = found[f"after-stat-{name}"]["stat"]
                after = {"exists": stat_info["exists"]}
                if stat_info["exists"]:
                    after["type"] = "directory" if stat_info["isdir"] else "link" if stat_info["islnk"] else "file"
                    after["mode"] = stat_info["mode"]
                content = found[f"after-content-{name}"]
                if not content.get("skipped"):
                    after["content"] = base64.b64decode(content["content"]).decode()
                if read_back[name]:
                    after["backup_content"] = base64.b64decode(found[f"after-backup-{name}"]["content"]).decode()
                result["_after"] = after
            if name in unit_back:
                result["_after"] = dict(line.split("=", 1) for line in found[f"after-unit-{name}"]["stdout"].splitlines())
        for key, want in dict({"failed": False}, **spec["branch"]).items():
            got = result.get(key, False if key == "failed" else None)
            if got != want:
                wrong.append(f"{name}: {key} is {got!r}, not {want!r}")
        for path, pattern in spec["patterns"].items():
            if not re.search(pattern, str(_at(uncut, path))):
                wrong.append(f"{name}: {path} {_at(uncut, path)!r} does not match {pattern}")
        results[name] = result
    if wrong:
        raise Failed("\n  ".join(["cases that did not land on the branch their name claims:", *wrong]))

    for name, result in results.items():
        write_json(os.path.join(DESTINATION, f"{name}.json"), result)
    write_json(os.path.join(DESTINATION, "index.json"), index)
    # The play itself, which golden.rs replays under Volant: the setup tasks, the read-backs and
    # the order the cases meet each other's state in are nowhere else.
    write_json(os.path.join(DESTINATION, "play.json"), redact.in_strings(play, to_placeholder), sort_keys=False, indent=1)
    for stale in sorted(set(os.listdir(DESTINATION)) - {f"{name}.json" for name in index} - {"index.json", "play.json"}):
        os.remove(os.path.join(DESTINATION, stale))
        print(f"removed module/{stale}: this run did not record it", file=sys.stderr)
    print(f"module goldens recorded: {len(index)} cases")
