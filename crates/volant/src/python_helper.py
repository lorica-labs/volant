# SPDX-License-Identifier: GPL-3.0-or-later
"""Builds one union zip holding every module a run needs, plus the shared module_utils.

It reads length-prefixed frames on stdin and answers on stdout, one frame each, and lives for
the whole run: ansible-core caches a module's zip by its name, so the first build of a module
costs 140 ms and every later one 2.8 ms. A helper restarted per task would pay the 140 ms
every time.

Entries shared between per-module zips are byte-identical today, which is what makes merging
them into one blob well defined. A conflict is therefore a hard error rather than a silent
last-writer-wins: shipping a blob whose module_utils came from whichever module was merged
last would run something nobody asked for.

Nothing but a frame is ever written to stdout; anything the helper wants to say goes to
stderr, which the controller passes through.
"""
import base64
import io
import json
import re
import struct
import sys
import zipfile

# The wrapper ansible-core emits carries the zip already base64 encoded, and the arguments
# outside it, so the blob is keyed by the module alone.
ZIP_DATA = re.compile(rb"zip_data='([^']+)'")
MODULE_FQN = re.compile(r"module_fqn='([^']+)'")
PROFILE = re.compile(r"profile='([^']+)'")
RLIMIT_NOFILE = re.compile(r"rlimit_nofile=(\d+)")
EXTENSIONS = re.compile(r"extensions=(\{.*?\})")


def facts(text):
    """The per-module facts a task needs alongside the shared blob."""
    return {
        "module_fqn": group(MODULE_FQN, text, "module_fqn"),
        "profile": group(PROFILE, text, "profile"),
        "rlimit_nofile": int(group(RLIMIT_NOFILE, text, "rlimit_nofile")),
        "extensions": json.loads(group(EXTENSIONS, text, "extensions").replace("'", '"')),
    }


def group(pattern, text, what):
    """The one value a field carries in the wrapper, refusing zero of them and refusing two.

    A missing match is loud already. A second one is not: the wrapper is a template around the
    call that carries these fields, and a template that ever names one of them before the call -
    a default in a signature is the obvious way - would have the module built with the
    template's value read back as its own, and the module then runs under something it was not
    built for.
    """
    found = pattern.findall(text)
    if not found:
        raise RuntimeError(
            "the wrapper ansible-core emitted carries no %s; this helper cannot read the "
            "payloads of this ansible-core" % what
        )
    if len(found) > 1:
        raise RuntimeError(
            "the wrapper ansible-core emitted carries %d values for %s; this helper cannot "
            "tell which one the module was built with" % (len(found), what)
        )
    return found[0]


def merge(entries, name, raw):
    """Folds one module's zip into the union, refusing an entry that differs."""
    archive = zipfile.ZipFile(io.BytesIO(raw))
    for entry in archive.namelist():
        if entry.endswith("/"):
            continue
        data = archive.read(entry)
        if entry in entries and entries[entry] != data:
            raise RuntimeError(
                "module_utils entry %r differs between %s and an earlier module; "
                "the union blob is only sound while they are identical" % (entry, name)
            )
        entries[entry] = data


def pack(entries):
    """The union zip, whose bytes are the same on every controller so they can name it.

    Sorted entries, a fixed timestamp, and the two fields `ZipInfo` otherwise takes from the
    machine doing the packing: `create_system` is 0 on Windows and 3 everywhere else, and
    `external_attr` is left at whatever `writestr` defaults to. Both land in the central
    directory, so leaving them alone gives the same blob a different hash per controller, and
    an agent cache that never hits.
    """
    buf = io.BytesIO()
    with zipfile.ZipFile(buf, "w", compression=zipfile.ZIP_STORED) as out:
        for entry in sorted(entries):
            info = zipfile.ZipInfo(filename=entry, date_time=(1980, 1, 1, 0, 0, 0))
            info.create_system = 3
            info.external_attr = 0o644 << 16
            out.writestr(info, entries[entry])
    return buf.getvalue()


def union(modules):
    """The union blob and the per-module facts, for every module named."""
    from ansible.executor import module_common
    from ansible.parsing.dataloader import DataLoader
    from ansible.plugins.loader import init_plugin_loader, module_loader
    from ansible.template import Templar

    init_plugin_loader()
    templar = Templar(loader=DataLoader())
    entries = {}
    facts_by_module = {}
    for name in modules:
        path = module_loader.find_plugin(name)
        if path is None:
            raise RuntimeError("ansible-core has no module %r" % name)
        try:
            # The interpreter in `task_vars` only shapes the shebang of a wrapper this path
            # never ships: the agent runs the module under the interpreter its own host
            # resolved. The zip itself is the same bytes whatever is named here.
            built = module_common.modify_module(
                module_name=name,
                module_path=path,
                module_args={},
                templar=templar,
                task_vars={"ansible_python_interpreter": "/usr/bin/python3"},
            )
        except Exception as failure:
            raise RuntimeError("building module %r: %s" % (name, failure))
        # A module built `old` has no zip at all: its whole body is the payload, and what makes
        # it work is an action plugin on the controller. The pre-flight refuses those by name
        # already; this is the belt to that pair of braces.
        if built.module_style != "new":
            raise RuntimeError(
                "module %r is built as %r, which has no module_utils zip to ship"
                % (name, built.module_style)
            )
        # `validate=True` so a byte that is not base64 raises here rather than being dropped
        # on the way to a zip that then opens short of an entry.
        zip_data = group(ZIP_DATA, built.b_module_data, "zip_data of %r" % name)
        merge(entries, name, base64.b64decode(zip_data, validate=True))
        facts_by_module[name] = facts(built.b_module_data.decode())
    blob = pack(entries)
    return {
        "zip_b64": base64.b64encode(blob).decode(),
        "modules": facts_by_module,
    }


def read_frame(stream):
    header = stream.read(4)
    if len(header) < 4:
        return None
    length = struct.unpack(">I", header)[0]
    payload = stream.read(length)
    if len(payload) < length:
        return None
    return json.loads(payload)


def write_frame(stream, message):
    payload = json.dumps(message).encode()
    stream.write(struct.pack(">I", len(payload)))
    stream.write(payload)
    stream.flush()


def main():
    while True:
        request = read_frame(sys.stdin.buffer)
        if request is None:
            return
        try:
            answer = union(request["modules"])
        except Exception as failure:
            answer = {"error": "%s: %s" % (type(failure).__name__, failure)}
        write_frame(sys.stdout.buffer, answer)


def self_check():
    """Everything the helper does that does not need ansible-core installed."""
    def one_entry(content):
        buf = io.BytesIO()
        with zipfile.ZipFile(buf, "w") as out:
            out.writestr("ansible/module_utils/basic.py", content)
        return buf.getvalue()

    packed = pack({"b.py": "second", "a.py": "first"})
    assert packed == pack({"a.py": "first", "b.py": "second"}), "the union zip is not ordered"
    inside = zipfile.ZipFile(io.BytesIO(packed))
    assert inside.namelist() == ["a.py", "b.py"], inside.namelist()
    for info in inside.infolist():
        # These three are what the bytes of the zip carry beside the entries themselves, and
        # `ZipInfo` takes two of them from the machine that packs. Left alone, the same modules
        # pack into different bytes on a Windows controller and on a Linux one, which gives the
        # same blob two content addresses and an agent cache that never hits.
        assert info.create_system == 3, info.create_system
        assert info.external_attr == 0o644 << 16, info.external_attr
        assert info.date_time == (1980, 1, 1, 0, 0, 0), info.date_time

    entries = {}
    merge(entries, "ping", one_entry("shared"))
    merge(entries, "stat", one_entry("shared"))
    assert list(entries) == ["ansible/module_utils/basic.py"], entries
    try:
        merge(entries, "file", one_entry("different"))
    except RuntimeError as conflict:
        assert "basic.py" in str(conflict), conflict
        assert "file" in str(conflict), conflict
    else:
        raise AssertionError("a byte conflict was merged instead of refused")
    wrapper = (
        "ansible_module='ping', module_fqn='ansible.modules.ping', profile='legacy', "
        "rlimit_nofile=0, extensions={}, zip_data='YQ=='"
    )
    assert facts(wrapper) == {
        "module_fqn": "ansible.modules.ping",
        "profile": "legacy",
        "rlimit_nofile": 0,
        "extensions": {},
    }, facts(wrapper)
    try:
        facts("nothing of the sort")
    except RuntimeError as unreadable:
        assert "module_fqn" in str(unreadable), unreadable
    else:
        raise AssertionError("an unreadable wrapper was accepted")
    try:
        facts("profile='template', " + wrapper)
    except RuntimeError as twice:
        assert "profile" in str(twice), twice
    else:
        raise AssertionError("a wrapper naming a field twice was read from its first match")


if __name__ == "__main__":
    if "--self-check" in sys.argv:
        self_check()
    else:
        main()
