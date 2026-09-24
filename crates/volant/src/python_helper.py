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
import importlib.util
import io
import json
import os
import re
import struct
import sys
import time
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


def merge(entries, owners, name, raw):
    """Folds one module's zip into the union, refusing an entry that differs.

    `owners` remembers which module brought each entry, so a conflict names both modules: a
    collection's module_utils land under `ansible_collections/` beside ansible-core's own, and
    the operator has to know which two modules disagree to drop one of them.
    """
    archive = zipfile.ZipFile(io.BytesIO(raw))
    for entry in archive.namelist():
        if entry.endswith("/"):
            continue
        data = archive.read(entry)
        if entry in entries and entries[entry] != data:
            raise RuntimeError(
                "module_utils entry %r differs between %s and %s; "
                "the union blob is only sound while they are identical"
                % (entry, owners[entry], name)
            )
        entries[entry] = data
        owners.setdefault(entry, name)


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


LOADED = []


def loaders():
    """`init_plugin_loader()`, once per helper: a second call warns on stderr that the
    collection finder is already configured, and a run that resolves and then builds asks twice.
    """
    if not LOADED:
        from ansible.plugins.loader import init_plugin_loader

        init_plugin_loader()
        LOADED.append(True)


def build(name):
    """One module's zip and facts, or RuntimeError saying why it cannot be shipped.

    Looked up with `mod_type=".py"`: a Windows module is a `.ps1` beside a `.py` that holds only
    its documentation, and the loader left to itself returns whichever it meets first.
    """
    from ansible.executor import module_common
    from ansible.parsing.dataloader import DataLoader
    from ansible.plugins.loader import module_loader
    from ansible.template import Templar

    loaders()
    path = module_loader.find_plugin(name, mod_type=".py")
    if path is None:
        raise RuntimeError("ansible-core has no module %r" % name)
    try:
        # The interpreter in `task_vars` only shapes the shebang of a wrapper this path never
        # ships: the agent runs the module under the interpreter its own host resolved. The zip
        # itself is the same bytes whatever is named here.
        built = module_common.modify_module(
            module_name=name,
            module_path=path,
            module_args={},
            templar=Templar(loader=DataLoader()),
            task_vars={"ansible_python_interpreter": "/usr/bin/python3"},
        )
    except Exception as failure:
        raise RuntimeError("building module %r: %s" % (name, failure))
    # A module built `old` has no zip at all: its whole body is the payload, and what makes it
    # work is an action plugin on the controller, or it is a documentation stub. `resolve`
    # answers those as unusable before a task can name them; this is the belt to that pair of
    # braces.
    if built.module_style != "new":
        raise RuntimeError(
            "module %r is built as %r, which has no module_utils zip to ship"
            % (name, built.module_style)
        )
    # `validate=True` so a byte that is not base64 raises here rather than being dropped on the
    # way to a zip that then opens short of an entry.
    zip_data = group(ZIP_DATA, built.b_module_data, "zip_data of %r" % name)
    # Whether the file is ansible-core's own module. Measured on 2.19.12, a `library/stat.py` is
    # built as `ansible.legacy.stat` and a collection's module under `ansible_collections.`; this
    # is the controller's own word, whatever a wrapper names a module next.
    import ansible.modules

    core = os.path.dirname(os.path.realpath(path)) == os.path.dirname(
        os.path.realpath(ansible.modules.__file__)
    )
    found = facts(built.b_module_data.decode())
    found["core"] = core
    return base64.b64decode(zip_data, validate=True), found


def union(modules):
    """The union blob, the per-module facts, and the files they came from, for every module
    named."""
    import ansible

    entries = {}
    owners = {}
    facts_by_module = {}
    for name in modules:
        raw, found = build(name)
        merge(entries, owners, name, raw)
        facts_by_module[name] = found
    blob = pack(entries)
    track_core()
    for entry in entries:
        parts = entry.split("/")
        if parts[0] == "ansible_collections" and len(parts) > 3:
            track_collection(parts[1] + "." + parts[2])
    track_overrides(entries, *override_paths())
    mapped = copies(entries, os.path.dirname(ansible.__path__[0]), collection_dir)
    return {
        "zip_b64": base64.b64encode(blob).decode(),
        "modules": facts_by_module,
        "sources": sources(time.time_ns()) if mapped else None,
        # The interpreter that built this, whatever launched it: a shim or a wrapper is
        # something else, and the controller files the union only under the one that ran.
        "interpreter": os.path.realpath(sys.executable),
    }


# Every file an answer depended on, by absolute path: (st_size, st_mtime_ns). The controller keeps
# a union between runs only while each of them still has both. A file is stat-ed before it is read,
# so one rewritten in between shows a newer mtime than the one kept here, never an older one.
TRACKED = {}


def track(path):
    """Records `path`, or, when it does not exist, the nearest directory above it that does:
    creating the path later adds an entry to that directory, which moves its mtime."""
    path = os.path.abspath(path)
    while True:
        try:
            st = os.stat(path)
        except OSError:
            parent = os.path.dirname(path)
            if parent == path:
                return
            path = parent
            continue
        TRACKED[path] = (st.st_size, st.st_mtime_ns)
        return


def track_core():
    """ansible-core itself: an upgrade rewrites `release.py`, the package `__init__.py` files a
    zip carries without a file of their own come from `module_common.py`, and an `ansible`
    package appearing earlier on `sys.path` adds an entry to a directory recorded here."""
    import ansible

    root = ansible.__path__[0]
    track(os.path.join(root, "release.py"))
    track(os.path.join(root, "executor", "module_common.py"))
    for entry in sys.path:
        track(os.path.join(entry, "ansible"))
    # The user site joins `sys.path` only once it exists, so it is looked for even when absent:
    # `pip install --user ansible-core` would otherwise shadow this one unseen.
    import site

    if site.ENABLE_USER_SITE:
        track(os.path.join(site.getusersitepackages(), "ansible"))


def track_collection(collection):
    """Every place `collection` could be installed: installing it, upgrading it, or shadowing
    it from a path searched earlier rewrites one of these files or the directory above them.

    The configured paths, and every `sys.path` entry when ansible-core scans them: the finder
    lists only the entries that already had an `ansible_collections` directory, and `pip install
    ansible` into the interpreter's own site-packages creates one later.
    """
    from ansible import constants as C
    from ansible.utils.collection_loader import AnsibleCollectionConfig

    bases = list(AnsibleCollectionConfig.collection_paths)
    if C.COLLECTIONS_SCAN_SYS_PATH:
        bases.extend(sys.path)
    track_collection_in(collection, bases)


def track_collection_in(collection, bases):
    """`track_collection` over the directories `bases`."""
    if collection in ("ansible.builtin", "ansible.legacy"):
        return
    for base in bases:
        home = os.path.join(base, "ansible_collections", *collection.split("."))
        for name in ("MANIFEST.json", "FILES.json", os.path.join("meta", "runtime.yml")):
            track(os.path.join(home, name))


def override_paths():
    """The directories searched for a module and for a `module_utils` before ansible-core's own:
    `~/.ansible/plugins/modules`, `/usr/share/ansible/plugins/modules` and the `module_utils`
    pair by default, or what `library` and `module_utils` configure, absent ones included."""
    import ansible
    from ansible.plugins.loader import module_loader, module_utils_loader

    core = ansible.__path__[0] + os.sep

    def outside(paths):
        return [
            path
            for path in paths
            if not os.path.abspath(path).startswith(core)
            and os.path.basename(path) != "__pycache__"
        ]

    return (
        outside(module_loader._get_paths()),
        outside(module_utils_loader._get_paths(subdirs=False)),
    )


def track_overrides(entries, module_paths, utils_paths):
    """Each directory where a file dropped later would take the place of one this union holds:
    every module search path, and for `ansible/module_utils/a/b.py` each `module_utils` path and
    its `a` below it. A file added there moves that directory's mtime; an absent one is recorded
    as its nearest existing parent."""
    for path in module_paths:
        track(path)
    for name in entries:
        parts = name.split("/")
        if parts[:2] != ["ansible", "module_utils"]:
            continue
        for path in utils_paths:
            for depth in range(2, len(parts)):
                track(os.path.join(path, *parts[2:depth]))


def copies(entries, root, home_of):
    """Whether every zip entry is the copy of a file this can name, recording each file.

    `ansible/...` is read under `root`, the directory `ansible` was imported from, and
    `ansible_collections/<ns>/<name>/...` under `home_of("<ns>.<name>")`. The package
    `__init__.py` files ansible-core writes itself are let through: `module_common.py`, which
    `track_core` covers, writes `ansible/` and `ansible/module_utils/`, and every collection package
    down to `plugins/<kind>/` (fewer than six dotted parts, `CollectionModuleUtilLocator`). Anything
    else - a deeper package `__init__.py` ansible-core copies, a `module_utils` from a configured
    directory, a module from a `library` path - answers False, and the union is then rebuilt on
    every run rather than kept on a guess.
    """
    for name, data in entries.items():
        parts = name.split("/")
        path = None
        if parts[0] == "ansible":
            path = os.path.join(root, *parts)
        elif parts[0] == "ansible_collections" and len(parts) > 3:
            home = home_of(parts[1] + "." + parts[2])
            path = os.path.join(home, *parts[3:]) if home else None
        if path is not None and copied(path, data):
            continue
        if not generated(parts):
            return False
    return True


def generated(parts):
    """Whether the zip entry `parts` is a package `__init__.py` ansible-core writes itself."""
    if parts in (["ansible", "__init__.py"], ["ansible", "module_utils", "__init__.py"]):
        return True
    return parts[0] == "ansible_collections" and parts[-1] == "__init__.py" and len(parts) <= 6


def copied(path, data):
    """Whether `path` holds `data`, recording it when it does."""
    try:
        st = os.stat(path)
        with open(path, "rb") as source:
            same = source.read() == data
    except OSError:
        return False
    if same:
        TRACKED[path] = (st.st_size, st.st_mtime_ns)
    return same


def sources(now_ns):
    """What `TRACKED` holds, as the answer carries it, or None when a path is not UTF-8, which
    the frame's JSON cannot carry, or when a file changed less than two seconds before `now_ns`.

    The second rule is git's racy-timestamp one: a file rewritten after it was read but within
    the same tick of a coarse clock (4 ms on ext4, 1 s on NFS or HFS+, 2 s on FAT) keeps its size
    and mtime, and the entry would validate against the new file for good. The next run keeps it.
    """
    try:
        for path in TRACKED:
            path.encode("utf-8")
    except UnicodeEncodeError:
        return None
    if any(mtime > now_ns - 2_000_000_000 for _, mtime in TRACKED.values()):
        return None
    return [
        {"path": path, "len": size, "mtime_ns": mtime}
        for path, (size, mtime) in sorted(TRACKED.items())
    ]


def collection_dir(collection):
    """Where an installed collection lives, or None when none by that name is installed.

    Asked of the collection finder `init_plugin_loader()` installs, which reads the configured
    collection paths and nothing else: nothing is fetched to answer.
    """
    try:
        spec = importlib.util.find_spec("ansible_collections." + collection)
    except ModuleNotFoundError:
        return None
    if spec is None or not spec.submodule_search_locations:
        return None
    return list(spec.submodule_search_locations)[0]


def version_of(directory):
    """A collection's version from its MANIFEST.json, `*` when it has none, as ansible-galaxy
    lists a collection checked out rather than installed."""
    try:
        with open(os.path.join(directory, "MANIFEST.json")) as manifest:
            return json.load(manifest)["collection_info"]["version"]
    except (OSError, KeyError, ValueError):
        return "*"


def collection_of(name):
    """The `namespace.collection` a dotted module name lives in, or None for a shorter name."""
    parts = name.split(".")
    return ".".join(parts[:2]) if len(parts) > 2 else None


def resolve(names):
    """What ansible-core makes of each name, one answer per name.

    Each name is answered on its own, so one that raises - a `runtime.yml` tombstone raises
    `AnsiblePluginRemovedError` - is that name's answer and not the whole run's: the pre-flight
    refuses it only if a task names it.
    """
    loaders()
    track_core()
    answers = {}
    for name in names:
        try:
            answers[name] = resolve_one(name)
        except Exception as failure:
            answers[name] = {"unusable": "%s: %s" % (type(failure).__name__, failure)}
    return {"resolved": answers}


def resolve_one(name):
    """A module it can build, a module its collection runs through an action plugin, a name it
    knows and cannot run, or nothing, and then which collection would have to be installed.

    The collection is looked for before the module: asking the module loader for a name in a
    collection that is not there prints a loader warning the operator has no use for.
    """
    from ansible.plugins.loader import action_loader, module_loader

    collection = collection_of(name)
    if collection is not None:
        track_collection(collection)
        if collection_dir(collection) is None:
            return {"missing": collection}
    module = module_loader.find_plugin_with_context(name, mod_type=".py")
    for hop in module.redirect_list:
        if collection_of(hop) is not None:
            track_collection(collection_of(hop))
    if not module.resolved:
        # A `runtime.yml` redirect into a collection nobody installed: the install hint names
        # the collection at the end of the chain, not the one the playbook wrote.
        last = module.redirect_list[-1] if module.redirect_list else name
        hop = collection_of(last)
        if last != name and hop is not None and collection_dir(hop) is None:
            return {"missing": hop}
        return {"missing": None}
    # As `TaskExecutor._get_action_handler_with_module_context` reads it: the action plugin
    # `runtime.yml` routes the module to comes first, then one of the module's own name. Every
    # module resolves to `normal` when nothing else claims it, so that one is the absence of an
    # action plugin, not one.
    if module.action_plugin:
        return {"action_plugin": module.resolved_fqcn}
    action = action_loader.find_plugin_with_context(name)
    if action.resolved and action.plugin_resolved_name != "ansible.builtin.normal":
        return {"action_plugin": module.resolved_fqcn}
    try:
        build(name)
    except RuntimeError as unbuildable:
        return {"unusable": str(unbuildable)}
    owner = module.plugin_resolved_collection
    where = collection_dir(owner) if owner and owner != "ansible.builtin" else None
    return {
        "module": module.resolved_fqcn,
        "collection": [owner, version_of(where)] if where else None,
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
    # The frames keep the real stdout. Anything else that prints - the plugin loader, a
    # collection imported under it - goes to stderr, which the controller passes through.
    frames = sys.stdout.buffer
    sys.stdout = sys.stderr
    # `python -c` puts the working directory first on the path, which ansible-playbook, a script,
    # never does: an `ansible` directory where the run starts is not the ansible-core to build
    # with, and the working directory is no file a kept union should depend on.
    sys.path[:] = [entry for entry in sys.path if entry]
    while True:
        request = read_frame(sys.stdin.buffer)
        if request is None:
            return
        try:
            if "resolve" in request:
                answer = resolve(request["resolve"])
            else:
                answer = union(request["modules"])
        except Exception as failure:
            answer = {"error": "%s: %s" % (type(failure).__name__, failure)}
        write_frame(frames, answer)


def self_check():
    """Everything the helper does that does not need ansible-core installed."""
    def one_entry(content, entry="ansible/module_utils/basic.py"):
        buf = io.BytesIO()
        with zipfile.ZipFile(buf, "w") as out:
            out.writestr(entry, content)
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

    entries, owners = {}, {}
    merge(entries, owners, "ping", one_entry("shared"))
    merge(entries, owners, "stat", one_entry("shared"))
    assert list(entries) == ["ansible/module_utils/basic.py"], entries
    try:
        merge(entries, owners, "file", one_entry("different"))
    except RuntimeError as conflict:
        assert "basic.py" in str(conflict), conflict
        assert "file" in str(conflict), conflict
        assert "ping" in str(conflict), conflict
    else:
        raise AssertionError("a byte conflict was merged instead of refused")
    # The same guard for a collection's module_utils, which land under `ansible_collections/`
    # beside ansible-core's own: two collection modules shipping different bytes for one entry.
    entries, owners = {}, {}
    shared = "ansible_collections/ansible/posix/plugins/module_utils/version.py"
    merge(entries, owners, "ansible.posix.sysctl", one_entry("2.2.2", shared))
    try:
        merge(entries, owners, "ansible.posix.firewalld", one_entry("2.2.3", shared))
    except RuntimeError as conflict:
        assert shared in str(conflict), conflict
        assert "ansible.posix.sysctl" in str(conflict), conflict
        assert "ansible.posix.firewalld" in str(conflict), conflict
    else:
        raise AssertionError("a byte conflict under ansible_collections/ was merged")
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
    check_sources()


def check_sources():
    """Each zip entry named back to the file it copies, and a union that has one entry this
    cannot name kept out of the cache."""
    import tempfile

    with tempfile.TemporaryDirectory() as root:
        core = os.path.join(root, "site", "ansible", "module_utils")
        home = os.path.join(root, "coll", "ns", "c")
        os.makedirs(core)
        os.makedirs(os.path.join(home, "plugins", "modules"))
        with open(os.path.join(core, "basic.py"), "wb") as out:
            out.write(b"basic")
        with open(os.path.join(home, "plugins", "modules", "m.py"), "wb") as out:
            out.write(b"module")
        homes = {"ns.c": home}.get
        entries = {
            "ansible/module_utils/basic.py": b"basic",
            "ansible_collections/ns/c/plugins/modules/m.py": b"module",
            # Written by module_common.py, with no file of their own.
            "ansible/__init__.py": b"from pkgutil import extend_path\n",
            "ansible_collections/__init__.py": b"",
            "ansible_collections/ns/c/plugins/__init__.py": b"",
        }
        TRACKED.clear()
        site = os.path.join(root, "site")
        assert copies(entries, site, homes), TRACKED
        assert sorted(TRACKED) == [
            os.path.join(home, "plugins", "modules", "m.py"),
            os.path.join(core, "basic.py"),
        ], TRACKED
        assert TRACKED[os.path.join(core, "basic.py")][0] == 5, TRACKED
        for odd in (
            {"ansible/module_utils/basic.py": b"other"},
            {"ansible/module_utils/custom.py": b"from a module_utils directory"},
            {"ansible_collections/no/such/plugins/modules/m.py": b"module"},
            {"elsewhere/x.py": b""},
            # A package init ansible-core copies from disk rather than writes: one saved during
            # the build no longer matches, and has to keep the union out rather than go unwatched.
            {"ansible/module_utils/common/__init__.py": b"old"},
            {"ansible_collections/ns/c/plugins/module_utils/net/__init__.py": b"old"},
        ):
            assert not copies(odd, site, homes), odd
        # A path that is not there is recorded as the nearest directory above it that is.
        TRACKED.clear()
        track(os.path.join(home, "FILES.json"))
        track(os.path.join(root, "coll", "absent", "c", "MANIFEST.json"))
        assert sorted(TRACKED) == [os.path.join(root, "coll"), home], TRACKED
        # Every file here was written a moment ago: too recent to vouch for.
        now = time.time_ns()
        assert sources(now) is None, TRACKED
        assert sources(now + 3_000_000_000) == [
            {"path": path, "len": size, "mtime_ns": mtime}
            for path, (size, mtime) in sorted(TRACKED.items())
        ]
        # A collection looked for under a `sys.path` entry that has no `ansible_collections` yet
        # is recorded as that entry, which `pip install ansible` then writes into.
        TRACKED.clear()
        track_collection_in("community.general", [site])
        assert sorted(TRACKED) == [site], TRACKED
        # A module search path that is not there is its nearest parent; a nested `module_utils`
        # import is watched in its own directory below each `module_utils` path.
        TRACKED.clear()
        utils = os.path.join(root, "utils")
        os.makedirs(os.path.join(utils, "net"))
        track_overrides(
            {"ansible/module_utils/net/a.py": b"", "ansible/modules/stat.py": b""},
            [os.path.join(root, "plugins", "modules")],
            [utils],
        )
        assert sorted(TRACKED) == [root, utils, os.path.join(utils, "net")], TRACKED
        TRACKED.clear()


if __name__ == "__main__":
    if "--self-check" in sys.argv:
        self_check()
    else:
        main()
