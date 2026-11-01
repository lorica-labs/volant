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
import collections.abc
import contextlib
import datetime
import importlib.util
import io
import json
import os
import re
import struct
import sys
import threading
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
        scan_once()
        LOADED.append(True)


def scan_once():
    """Has ansible-core read each module_utils file once per helper rather than once per module.

    Building a module walks every module_utils file it imports: finds the file, parses it, lists
    its imports. ansible-core does that walk again for every module, and the modules of one run
    share most of it (`basic` and the 50 files under it), so a union of 18 modules spent 3.9 s
    there out of 4. Each of the three steps answers from its arguments alone, the files are not
    rewritten while a helper builds, and the zip holds the same files with the same bytes: what
    a step answered for one module is kept and handed to the next. Measured on 2.19.12, the 18
    modules build in 1.3 s.

    A step whose arguments cannot be a key, or a release that moved one of these names, is left
    as ansible-core wrote it.
    """
    import ast

    from ansible.executor import module_common

    def key_of(values):
        # A syntax tree is the same tree only as the same object, which the kept answer holds
        # alive so its id is not reused.
        return tuple(
            ("tree", id(v)) if isinstance(v, ast.AST)
            else tuple(v) if isinstance(v, list)
            else v
            for v in values
        )

    def kept(compute):
        answers = {}

        def answer(*args, **kwargs):
            key = key_of(args) + tuple((k,) + key_of((v,)) for k, v in sorted(kwargs.items()))
            try:
                hash(key)
            except TypeError:
                return compute(*args, **kwargs)
            if key not in answers:
                answers[key] = (args, kwargs, compute(*args, **kwargs))
            return answers[key][2]

        return answer

    def kept_init(cls):
        # What the caller reads is the object, so the answer kept is its attributes.
        init = cls.__init__

        def state(*args, **kwargs):
            fresh = object.__new__(cls)
            init(fresh, *args, **kwargs)
            return dict(fresh.__dict__)

        state = kept(state)

        def __init__(self, *args, **kwargs):
            self.__dict__.update(state(*args, **kwargs))

        cls.__init__ = __init__

    names = ("ModuleDepFinder", "LegacyModuleUtilLocator", "CollectionModuleUtilLocator")
    if not callable(getattr(module_common, "_compile_module_ast", None)) or not all(
        isinstance(getattr(module_common, n, None), type) for n in names
    ):
        return
    module_common._compile_module_ast = kept(module_common._compile_module_ast)
    for name in names:
        kept_init(getattr(module_common, name))


class ShippedAsItself(RuntimeError):
    """A module ansible-core builds with no zip: one in another language that reads its arguments
    from the file named on its command line (`WANT_JSON`), or an `old` one, a script reading them
    as `key=value` words from that file. It ships as itself: ansible-core's own `_execute_module`
    copies it and its arguments file to the host and runs it there."""


def shipped_as_itself(name):
    """Whether the module `name` is one `ShippedAsItself` stands for."""
    try:
        build(name)
    except ShippedAsItself:
        return True
    except Exception:
        return False
    return False


def find_module(name):
    """The module `name` resolves to, as a POSIX host runs it.

    Looked up with `mod_type=".py"`: a Windows module is a `.ps1` beside a `.py` that holds only
    its documentation, and the loader left to itself returns whichever it meets first. A file with
    no extension that comes first on the module path wins over that: measured on ansible-core
    2.19.12, a `ping` in `ANSIBLE_LIBRARY` runs in place of ansible-core's `ping.py`.
    """
    from ansible.plugins.loader import module_loader

    plain = module_loader.find_plugin_with_context(name, mod_type="")
    if plain.resolved and not os.path.splitext(plain.plugin_resolved_path)[1]:
        return plain
    found = module_loader.find_plugin_with_context(name, mod_type=".py")
    # A short name `plugin_filters_cfg` rejects is still the module an action plugin runs: the
    # reference's actions ask for `ansible.legacy.<name>`, which the filter does not hold.
    if not found.resolved and "." not in name:
        found = module_loader.find_plugin_with_context("ansible.legacy." + name, mod_type=".py")
    # A POSIX connection prefers `.py`, then any file of that name: measured, a
    # `library/helloworld.sh` runs as `helloworld`.
    if not found.resolved and plain.resolved and not plain.plugin_resolved_path.endswith(".ps1"):
        return plain
    return found


def build(name):
    """One module's zip and facts, or RuntimeError saying why it cannot be shipped."""
    from ansible.executor import module_common
    from ansible.parsing.dataloader import DataLoader
    from ansible.template import Templar

    loaders()
    found = find_module(name)
    path = found.plugin_resolved_path if found.resolved else None
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
    # Measured on ansible-core 2.19.12: a `library/a.py` that only prints its JSON runs, copied
    # with its arguments file. A documentation stub runs the same way there and says nothing.
    if built.module_style in ("non_native_want_json", "old"):
        raise ShippedAsItself(name)
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
        try:
            raw, found = build(name)
        except ShippedAsItself:
            raise RuntimeError(
                "module %r at %s is a script ansible-core copies to the host as it is, which "
                "Volant runs only under a name ansible-core does not ship a module for"
                % (name, find_module(name).plugin_resolved_path)
            )
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

    bases = [
        os.path.join(directory, "collections") for directory in AnsibleCollectionConfig.playbook_paths
    ] + list(AnsibleCollectionConfig.collection_paths)
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
    down to `plugins/<kind>/` (fewer than six dotted parts, `CollectionModuleUtilLocator`). It also
    writes an empty one for a deeper package directory that has none on disk, a namespace package
    such as community.general's `module_utils/_mh/`: that directory is recorded, since adding an
    `__init__.py` to it later moves its mtime. Anything else - a deeper package `__init__.py`
    ansible-core copies and that changed, a `module_utils` from a configured directory, a module
    from a `library` path - answers False, and the union is then rebuilt on every run rather than
    kept on a guess.
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
        if generated(parts):
            continue
        if path is not None and namespace_init(path, data):
            track(os.path.dirname(path))
            continue
        return False
    return True


def namespace_init(path, data):
    """Whether `data` at `path` is the empty `__init__.py` module_common.py writes for a package
    directory that exists without one."""
    return (
        data == b""
        and os.path.basename(path) == "__init__.py"
        and not os.path.lexists(path)
        and os.path.isdir(os.path.dirname(path))
    )


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
    """The `namespace.collection` a dotted module name lives in, or None for a shorter name and
    for `ansible.legacy`, which is no collection to install: it names what the module paths and
    the `library/` directories hold, then ansible-core's own."""
    parts = name.split(".")
    if len(parts) < 3 or parts[:2] == ["ansible", "legacy"]:
        return None
    return ".".join(parts[:2])


def resolve(names):
    """What ansible-core makes of each name, one answer per name.

    Each name is answered on its own, so one that raises - a `runtime.yml` tombstone raises
    `AnsiblePluginRemovedError` - is that name's answer and not the whole run's: the pre-flight
    refuses it only if a task names it.

    The loader's warning about a collection nobody installed is not printed: the engine says it
    when a task needs the name, which a task its `when` skips never does.
    """
    from ansible.utils.display import Display

    loaders()
    track_core()
    display = Display()
    said = display.error_as_warning
    display.error_as_warning = lambda *args, **kwargs: None
    answers = {}
    try:
        for name in names:
            try:
                answers[name] = resolve_one(name)
            except Exception as failure:
                answers[name] = {"unusable": "%s: %s" % (type(failure).__name__, failure)}
    finally:
        display.error_as_warning = said
    return {"resolved": answers}


def resolve_one(name):
    """A module it can build, a module its collection runs through an action plugin, a name it
    knows and cannot run, or nothing, and then which collection would have to be installed.

    The collection is looked for before the module: asking the module loader for a name in a
    collection that is not there prints a loader warning the operator has no use for.
    """
    from ansible.plugins.loader import action_loader

    collection = collection_of(name)
    if collection is not None:
        track_collection(collection)
        if collection_dir(collection) is None:
            return {"missing": collection}
    module = find_module(name)
    for hop in module.redirect_list:
        if collection_of(hop) is not None:
            track_collection(collection_of(hop))
    if not module.resolved:
        # An action plugin with no module of its name: the reference refuses a task only when
        # neither loader answers to it.
        action = action_loader.find_plugin_with_context(name)
        if action.resolved and action.plugin_resolved_name != "ansible.builtin.normal":
            return {"action_plugin": action.resolved_fqcn, "with_module": False}
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
    handler = module.action_plugin
    if not handler:
        action = action_loader.find_plugin_with_context(name)
        if action.resolved and action.plugin_resolved_name != "ansible.builtin.normal":
            handler = action.resolved_fqcn
    if handler:
        # The plugin that runs is named, and whether the module of the asked name builds: a
        # plugin may run it (`synchronize`), and then the union has to hold it.
        try:
            build(name)
            built = True
        except Exception:
            built = False
        return {"action_plugin": handler, "with_module": built}
    try:
        build(name)
    except ShippedAsItself:
        # Run by ansible-core's `normal` action plugin, as the reference runs every module with
        # no plugin of its own.
        return {"action_plugin": "ansible.builtin.normal", "with_module": False}
    except RuntimeError as unbuildable:
        return {"unusable": str(unbuildable)}
    owner = module.plugin_resolved_collection
    where = collection_dir(owner) if owner and owner != "ansible.builtin" else None
    return {
        "module": module.resolved_fqcn,
        "collection": [owner, version_of(where)] if where else None,
    }


def plugin_dirs(dirs, module_path=()):
    """Adds each playbook's and each role's directory as ansible-playbook adds it, with
    `add_all_plugin_dirs`: its `library/`, `module_utils/` and other plugin directories are
    searched before the configured paths, in the order given, so a `library/stat.py` beside the
    playbook is the `stat` a task gets. The `-M` directories lead the configured paths."""
    from ansible.plugins.loader import add_all_plugin_dirs, module_loader
    from ansible.utils.collection_loader import AnsibleCollectionConfig

    loaders()
    if module_path:
        # `-M` is the front of the configured module path, which every playbook's and role's
        # `library/` still precedes: measured on 2.19.12, a `library/mymod.py` beside the
        # playbook wins over `-M`.
        configured = [p for p in (module_loader.config or []) if p not in module_path]
        module_loader.config = list(module_path) + configured
        module_loader._clear_caches()
    for directory in dirs:
        add_all_plugin_dirs(directory)
    # A `collections/` directory beside a playbook is searched for collections, as
    # ansible-playbook searches it.
    AnsibleCollectionConfig.playbook_paths = list(dirs)
    return {}


def resolve_action(name):
    """The full name of the module or action plugin that answers `name`, or None, as
    `FieldAttributeBase._resolve_action` finds it."""
    from ansible.plugins.loader import action_loader, module_loader

    context = module_loader.find_plugin_with_context(name, ignore_deprecated=True)
    if context.resolved and not context.action_plugin:
        prefer = action_loader.find_plugin_with_context(name, ignore_deprecated=True)
        if prefer.resolved:
            context = prefer
    elif not context.resolved:
        context = action_loader.find_plugin_with_context(name, ignore_deprecated=True)
    return context.resolved_fqcn if context.resolved else None


def own_plugins():
    """The filters and tests defined outside ansible-core, in the playbook's own plugin
    directories or the configured ones, by short name: the loader finds them before its own."""
    import ansible
    from ansible.plugins.loader import filter_loader, test_loader

    loaders()
    core = os.path.dirname(os.path.realpath(ansible.__file__)) + os.sep
    return {
        kind: sorted(
            {
                plugin.ansible_name
                for plugin in loader.all()
                if not os.path.realpath(plugin._original_path).startswith(core)
            }
        )
        for kind, loader in (("filter", filter_loader), ("test", test_loader))
    }


def actions(names):
    """The full name of the action or module each `module_defaults` key spells, or None."""
    loaders()
    return {name: resolve_action(name) for name in names}


def connections(names):
    """What ansible-core's connection loader makes of each name: the plugin's full name and the
    names it went through, or `missing` with the warning the loader gave, or why the loader
    refused the name. That warning is returned rather than printed: the engine prints it when it
    fails the task, as the reference does."""
    from ansible.plugins.loader import connection_loader
    from ansible.utils.display import Display

    loaders()
    display = Display()
    said = display.error_as_warning
    caught = []
    display.error_as_warning = lambda msg, exception, *args, **kwargs: caught.append(
        "%s: %s" % (msg.rstrip("."), exception)
    )
    answers = {}
    try:
        for name in names:
            del caught[:]
            try:
                found = connection_loader.find_plugin_with_context(name)
            except Exception as failure:
                answers[name] = {"unusable": "%s: %s" % (type(failure).__name__, failure)}
                continue
            if found.resolved:
                answers[name] = {"fqcn": found.resolved_fqcn, "hops": list(found.redirect_list)}
            else:
                answers[name] = {"missing": True, "warning": caught[0] if caught else None}
    finally:
        display.error_as_warning = said
    return answers


def diff_text(diffs, colored=False):
    """The text `ansible-playbook` prints for a result's `diff`: ansible-core's own
    `CallbackBase._get_diff`, which colours the lines difflib made and nothing else."""
    from ansible.plugins.callback import CallbackBase
    from ansible.utils import color

    color.ANSIBLE_COLOR = bool(colored)
    return CallbackBase()._get_diff(diffs)


def action_groups(groups):
    """The modules each `module_defaults` group holds, as `FieldAttributeBase._resolve_group`
    resolves them: the group is declared in its collection's `action_groups`, its members are
    resolved to the module or action plugin that answers them, and `extend_group` brings in the
    members of other groups, which may be absent. A group nobody declares is answered as missing.
    """
    from ansible.utils.collection_loader import AnsibleCollectionRef
    from ansible.utils.collection_loader._collection_finder import _get_collection_metadata

    loaders()

    known = {}

    def full(name, collection):
        return name if AnsibleCollectionRef.is_valid_fqcr(name) else collection + "." + name

    def members(group):
        """The resolved actions of `group` (a full name), or None when nobody declares it."""
        collection = ".".join(group.split(".")[0:2])
        if group in known:
            return known[group]
        try:
            declared = _get_collection_metadata(collection).get("action_groups", {})
        except ValueError:
            return None
        entries = declared.get(group, declared.get(group.split(collection + ".")[-1]))
        if entries is None:
            return None
        actions = []
        extended = []
        for entry in entries:
            if not isinstance(entry, str):
                # Only the `metadata` entry is anything but a name, and only `extend_group` is
                # read from it.
                metadata = entry.get("metadata") if isinstance(entry, dict) else None
                if isinstance(metadata, dict):
                    extend = metadata.get("extend_group", [])
                    extended = [extend] if isinstance(extend, str) else extend
                    if not isinstance(extended, list):
                        extended = []
                continue
            resolved = resolve_action(full(entry, collection))
            if resolved:
                actions.append(resolved)
        known[group] = actions
        for other in extended:
            actions.extend(members(full(other, collection)) or [])
        return actions

    answers = {}
    for group in groups:
        found = members(group)
        answers[group] = {"actions": found} if found is not None else {"missing": True}
    return answers


CALL_NAME = re.compile(r"^[A-Za-z_][A-Za-z0-9_]*(\.[A-Za-z_][A-Za-z0-9_]*)*$")

# The only template text this helper ever renders is one of these, written here. What a play
# holds reaches the plugin as the variables below, which are untrusted and so never templated
# again: the controller has already rendered every term.
EXPRESSIONS = {
    "filter": "volant_args__[0] | %s(*volant_args__[1:], **volant_kwargs__)",
    "test": "volant_args__[0] is %s(*volant_args__[1:], **volant_kwargs__)",
    "lookup": "volant_marks__(lookup(volant_name__, *volant_args__, **volant_kwargs__))",
}

# What an undefined value travels as between the controller and this helper. Here it is
# ansible-core's marker of one, so a lookup answers it, or reads it, as it does in the reference.
UNDEFINED_MARK = "__volant_undefined__"


def with_markers(value, sent):
    """`value` from the controller, each mark of an undefined value made a marker, kept in
    `sent`."""
    if isinstance(value, dict):
        if set(value) == {UNDEFINED_MARK}:
            from ansible._internal._templating._jinja_common import UndefinedMarker

            marker = UndefinedMarker(hint=value[UNDEFINED_MARK], _no_template_source=True)
            sent.append(marker)
            return marker
        return {key: with_markers(item, sent) for key, item in value.items()}
    if isinstance(value, list):
        return [with_markers(item, sent) for item in value]
    return value


def without_markers(value, sent):
    """A lookup's answer, each marker of `sent` in it made the mark the controller reads back as
    undefined. A marker the plugin made itself stays, and fails the call as it always did."""
    if any(value is marker for marker in sent):
        return {UNDEFINED_MARK: value._undefined_message}
    if isinstance(value, dict):
        return {key: without_markers(item, sent) for key, item in value.items()}
    if isinstance(value, (list, tuple)):
        return [without_markers(item, sent) for item in value]
    return value


def plain(value):
    """`value` as JSON holds it: lazy containers, tuples and sets as lists and mappings, text
    subclasses (`Markup`, tagged strings) as `str`, dates as ISO text, bytes as UTF-8 text."""
    if value is None or isinstance(value, bool):
        return value
    # What the controller's JSON reader would refuse is refused here, for this call alone.
    if isinstance(value, int):
        if not -(2**63) <= value < 2**64:
            raise RuntimeError("the result holds the integer %s, beyond 64 bits" % value)
        return int(value)
    if isinstance(value, float):
        if value != value or value in (float("inf"), float("-inf")):
            raise RuntimeError("the result holds %r, which JSON cannot carry" % value)
        return float(value)
    if isinstance(value, str):
        try:
            value.encode("utf-8")
        except UnicodeEncodeError as failure:
            raise RuntimeError("the result holds text that is not valid Unicode: %s" % failure)
        return str.__str__(value)
    if isinstance(value, (bytes, bytearray)):
        try:
            return bytes(value).decode("utf-8")
        except UnicodeDecodeError as failure:
            raise RuntimeError("the result holds bytes that are not UTF-8: %s" % failure)
    if isinstance(value, (datetime.date, datetime.time)):
        return value.isoformat()
    messages = sys.modules.get("ansible.module_utils._internal._messages")
    if messages is not None and isinstance(value, messages.ErrorSummary):
        # A module's error kept in a result, as ansible-core renders it there: its traceback
        # when tracebacks are on, `(traceback unavailable)` otherwise.
        from ansible._internal._templating._transform import error_summary

        return error_summary(value)
    if isinstance(value, collections.abc.Mapping):
        return {
            str.__str__(k) if isinstance(k, str) else str(k): plain(v) for k, v in value.items()
        }
    if isinstance(value, (collections.abc.Iterable)):
        return [plain(item) for item in value]
    shown = summary_shown(value)
    if shown is not None:
        return plain(shown)
    raise RuntimeError("a value of type %s cannot be returned" % type(value).__name__)


def summary_shown(value):
    """A warning or a deprecation of ansible-core 2.19 as a registered result holds it: the
    warning's brief message, the deprecation's mapping; None for any other value."""
    from ansible._internal._templating import _transform
    from ansible.module_utils._internal import _messages

    if isinstance(value, _messages.DeprecationSummary):
        shown = _transform.deprecation_summary(value)
        deprecator = shown.get("deprecator")
        if deprecator is not None:
            kind = deprecator.type
            shown["deprecator"] = {
                "resolved_name": deprecator.resolved_name,
                "type": getattr(kind, "value", kind),
            }
        return shown
    if isinstance(value, _messages.WarningSummary):
        return _transform.warning_summary(value)
    return None


def marked(value):
    """`plain`, with each string the plugin did not trust as a template as
    `{"__ansible_unsafe": text}`, the shape `ansible-inventory --list` prints it in."""
    from ansible.template import is_trusted_as_template

    if isinstance(value, str):
        text = plain(value)
        return text if is_trusted_as_template(value) else {"__ansible_unsafe": text}
    if isinstance(value, collections.abc.Mapping):
        return {str.__str__(k) if isinstance(k, str) else str(k): marked(v) for k, v in value.items()}
    if isinstance(value, (list, tuple, set, frozenset)):
        return [marked(item) for item in value]
    return plain(value)


def inventory(path, plugin=None):
    """One collection inventory plugin reading `path`, as the reference's `auto` plugin runs it,
    inside the same `_InventoryDataWrapper` the inventory manager gives it (so a plugin's trust
    and origins are the reference's) and with its cache: the groups, children, hosts and host
    variables it made as plain JSON, and the lines it said. A failure is answered as data, so
    the warnings said before it are kept. `plugin` names an enabled plugin to run in place of
    `auto`, which answers `declined` when its `verify_file` refuses the source."""
    from ansible._internal._datatag._tags import Origin
    from ansible.inventory.manager import InventoryManager, _InventoryDataWrapper
    from ansible.parsing.dataloader import DataLoader
    from ansible.plugins.loader import inventory_loader
    from ansible.utils import display as display_module
    from ansible.vars.manager import VariableManager

    warnings = []
    verbose = []
    display = display_module.Display()
    said, spoke = display._warning, display.v
    format_message = display_module._display_utils.format_message
    display._warning = lambda warning: warnings.append(
        format_message(warning, False).rstrip(chr(10)) + chr(10))
    display.v = lambda message, *_, **__: verbose.append(str(message))
    try:
        loader = DataLoader()
        manager = InventoryManager(loader, sources=[path], parse=False)
        manager._inventory.current_source = path
        target = inventory_loader.get(plugin or "auto")
        if plugin and not target.verify_file(path):
            return {"declined": True, "verbose": verbose, "warnings": warnings}
        origin = Origin(
            description="<inventory plugin %r with source %r>" % (plugin or "auto", path)
        )
        wrapper = _InventoryDataWrapper(manager._inventory, target_plugin=target, origin=origin)
        target.parse(wrapper, loader, path, cache=True)
        data = manager._inventory
        # What `ansible-inventory --list` does next: the variables of each host, which is where
        # the reference warns about a variable that has a reserved name.
        variables = VariableManager(loader=loader, inventory=manager)
        for host in data.hosts.values():
            variables.get_vars(host=host, include_hostvars=False)
        magic = ("inventory_file", "inventory_dir")
        return {
            "verbose": verbose,
            "warnings": warnings,
            "groups": [
                {
                    "name": g.name,
                    "hosts": [h.name for h in g.hosts],
                    "children": [c.name for c in g.child_groups],
                    "vars": marked(g.vars),
                }
                for g in data.groups.values()
            ],
            "hosts": [{"name": h.name, "vars": marked({k: v for k, v in h.vars.items() if k not in magic})} for h in data.hosts.values()],
        }
    except Exception as failure:
        return {"verbose": verbose, "warnings": warnings, "error": str(failure)}
    finally:
        display._warning, display.v = said, spoke


def lookup_needs(name):
    """The variables the lookup `name` reads off its `variables` argument: the names it asks for
    by literal key, and the variables its settings name, or `all` when it lists them or hands
    them on, which this cannot follow. Read from the plugin's source, so that a call sends what
    is read and not every variable of the host."""
    import ast

    from ansible import constants
    from ansible.plugins.loader import lookup_loader

    nothing = {"names": [], "all": False}
    everything = {"names": [], "all": True}
    if not CALL_NAME.match(name):
        return nothing
    try:
        context = lookup_loader.find_plugin_with_context(name)
        if not context.resolved:
            return nothing
        with open(context.plugin_resolved_path, encoding="utf-8") as source:
            tree = ast.parse(source.read())
    except Exception:
        return everything
    names = set()
    try:
        names.update(constants.config.get_plugin_vars("lookup", context.plugin_resolved_name))
    except Exception:
        pass
    parents = {}
    for node in ast.walk(tree):
        for child in ast.iter_child_nodes(node):
            parents[child] = node

    def literal(node):
        return isinstance(node, ast.Constant) and isinstance(node.value, str)

    for function in ast.walk(tree):
        if not isinstance(function, (ast.FunctionDef, ast.AsyncFunctionDef)):
            continue
        if "variables" not in [a.arg for a in function.args.args + function.args.kwonlyargs]:
            continue
        for use in ast.walk(function):
            if not (isinstance(use, ast.Name) and use.id == "variables"):
                continue
            up = parents.get(use)
            if isinstance(up, ast.Subscript) and up.value is use and literal(up.slice):
                names.add(up.slice.value)
            elif isinstance(up, ast.Attribute) and up.value is use and up.attr in ("get", "pop"):
                call = parents.get(up)
                if not (isinstance(call, ast.Call) and call.args and literal(call.args[0])):
                    return everything
                names.add(call.args[0].value)
            elif isinstance(up, ast.Compare) and use in up.comparators and literal(up.left):
                names.add(up.left.value)
            elif isinstance(up, ast.keyword) and up.arg == "var_options":
                continue
            elif (
                isinstance(up, ast.Call)
                and isinstance(up.func, ast.Attribute)
                and up.func.attr == "find_file_in_search_path"
            ):
                continue
            else:
                return everything
    if "available_variables" in ast.unparse(tree):
        return everything
    return {"names": sorted(names), "all": False}


def call(request):
    """One filter, test or lookup, run through ansible-core's own Jinja environment so its
    argument handling, error text and `errors`/`wantlist` shaping are the reference's."""
    from ansible.parsing.dataloader import DataLoader
    from ansible.template import Templar, trust_as_template

    loaders()
    kind, name = request["kind"], request["name"]
    if kind == "inventory":
        return {"value": inventory(request["args"][0], request["kwargs"].get("plugin"))}
    if kind == "inventory_plugin":
        from ansible.plugins.loader import inventory_loader

        return {"value": inventory_loader.has_plugin(name)}
    if kind == "plugin_path":
        from ansible.plugins import loader as plugin_loader

        found = getattr(plugin_loader, "%s_loader" % request["args"][0]).find_plugin(name)
        return {"value": found}
    if kind == "diff":
        return {"value": diff_text(request["args"][0], request["kwargs"].get("color", False))}
    if kind == "action_groups":
        return {"value": action_groups(request["args"])}
    if kind == "actions":
        return {"value": actions(request["args"])}
    if kind == "own_plugins":
        return {"value": own_plugins()}
    if kind == "connections":
        return {"value": connections(request["args"])}
    if kind == "lookup_needs":
        return {"value": lookup_needs(request["args"][0])}
    if kind == "reset_persistent":
        return {"value": reset_persistent(request["args"])}
    if kind not in EXPRESSIONS:
        return {"error": {"kind": "plugin", "message": "unknown plugin kind '%s'" % kind}}
    # Jinja's own sentence for a filter or a test, ansible-core's for a lookup.
    sentence = "The lookup plugin '%s' was not found." if kind == "lookup" else (
        "No %s named '%%s'." % kind
    )
    missing = {"kind": "not_found", "message": sentence % name}
    if not CALL_NAME.match(name):
        return {"error": missing}
    args = request["args"]
    resolve_only = request.get("resolve_only", False)
    if kind != "lookup" and not args and not resolve_only:
        return {"error": {"kind": "plugin", "message": "a %s needs the value it is given" % kind}}
    expression = EXPRESSIONS[kind] if kind == "lookup" else EXPRESSIONS[kind] % name
    try:
        variables = {
            # A variable the controller could not render is its template, rendered when the plugin
            # reads it, so a failure is raised there as the reference raises it.
            key: trust_as_template(value["__volant_template__"])
            if isinstance(value, dict) and set(value) == {"__volant_template__"}
            else value
            for key, value in (request.get("vars") or {}).items()
        }
        # The loader a lookup resolves relative names and runs commands through, anchored where
        # the reference anchors it: the playbook's directory, else the first search path entry.
        loader = DataLoader()
        anchor = variables.get("playbook_dir") or next(iter(variables.get("ansible_search_path") or []), None)
        if isinstance(anchor, str) and anchor:
            loader.set_basedir(anchor)
        hosts = variables.get("hostvars")
        if isinstance(hosts, dict) and set(hosts) == {"__volant_hostvars__"}:
            variables["hostvars"] = PreloadedHostVars(hosts["__volant_hostvars__"], loader)
        templar = Templar(loader=loader, variables=variables)
        # The environment resolves a plugin on `in`, which may raise the not-found error or the
        # load error of a broken plugin file: both are answered below like a failed call.
        # The engine's environment, not `templar.environment`: ansible-core 2.19 warns on every
        # read of that attribute, and the warning would land in the run's output.
        environment = templar._engine.environment
        if kind != "lookup" and name not in getattr(environment, kind + "s"):
            return {"error": missing}
        # Whether the plugin exists, for a call the controller does not make because a value it
        # would be given is undefined: ansible-core resolves the name before it looks at those.
        if resolve_only:
            # What `Task.preprocess_data` asks of a `with_<name>` key: `name in lookup_loader`,
            # which follows the collections' redirects and the `lookup_plugins/` directories.
            if kind == "lookup":
                from ansible.plugins.loader import lookup_loader

                if name not in lookup_loader:
                    return {"error": missing}
            return {"value": None}
        kwargs, marks = request["kwargs"], None
        if kind == "lookup":
            from ansible._internal._templating._jinja_plugins import _DirectCall

            sent = []
            args, kwargs = with_markers(args, sent), with_markers(kwargs, sent)
            # Called by Jinja as it is (`_DirectCall`): through the usual path, a marker given to
            # a function is its answer, and the render would raise on it.
            marks = _DirectCall.mark(lambda value: without_markers(value, sent))
        value = templar.evaluate_expression(
            trust_as_template(expression),
            local_variables={
                "volant_args__": args,
                "volant_kwargs__": kwargs,
                "volant_name__": name,
                "volant_marks__": marks,
            },
        )
        return {"value": plain(value)}
    except Exception as failure:
        # Named rather than imported: the class lives in ansible-core's private templating
        # package, and its name is what 2.19 keeps stable.
        if type(failure).__name__ == "AnsibleTemplatePluginNotFoundError":
            return {"error": missing}
        error = {"kind": "plugin", "message": str(failure)}
        chain = blocks(failure)
        if len(chain) > 1:
            error["chain"] = chain
        return {"error": error}


def blocks(failure):
    """The messages of the blocks ansible-core prints for `failure`, one per `<<< caused by >>>`:
    a cause that adds no source context or help text, and has no chain of another kind, is part of
    the block it explains (`_event_formatting.format_event_verbose_message`). An empty list when
    this ansible-core's private events are not the ones read here."""
    try:
        from ansible._internal._errors._error_factory import ControllerEventFactory
        from ansible.module_utils._internal._event_utils import deduplicate_message_parts

        event = ControllerEventFactory.from_exception(failure, False)
        out = []
        while event:
            messages = [event.msg]
            chain = event.chain
            while chain and chain.follow:
                cause = chain.event
                if cause.events:
                    break
                if (cause.formatted_source_context or cause.help_text) and (
                    cause.formatted_source_context != event.formatted_source_context
                    or cause.help_text != event.help_text
                ):
                    break
                if cause.chain and chain.msg_reason != cause.chain.msg_reason:
                    break
                messages.append(cause.msg)
                chain = cause.chain
            out.append(deduplicate_message_parts(messages))
            event = chain.event if chain and chain.follow else None
        return out
    except Exception:
        return []


# The pipes `main` reads requests from and writes frames to: an action plugin's questions to the
# controller travel over them while its request is in hand.
PIPES = {}
# One question at a time: a plugin that asks from two threads would interleave the frames.
ASKING = threading.Lock()


def ask(ident, op, **fields):
    """One question to the controller while action `ident` runs, and its answer: `{"value": …}`
    or `{"error": text}`. The controller does what is asked over the host's own link."""
    fields["op"] = op
    with ASKING:
        write_frame(PIPES["frames"], {"id": ident, "ask": fields})
        reply = read_frame(PIPES["requests"])
    if reply is None:
        # The controller closed the pipe: nothing the plugin asks for can be answered any more.
        raise SystemExit(1)
    if reply.get("id") != ident or not isinstance(reply.get("reply"), dict):
        raise SystemExit(
            "the controller answered %r to a question of action %s" % (reply.get("id"), ident)
        )
    return reply["reply"]


SHIM = {}


def shim():
    """The stand-in connection an action plugin is given, and `ActionBase` turned toward it, made
    once ansible-core is imported. What a plugin asks of the host goes to the controller, which
    runs it over the host's own link; a member the corpus's plugins do not use is refused by name.
    """
    if SHIM:
        return SHIM
    from ansible._internal._errors import _captured
    from ansible.errors import AnsibleConnectionFailure, AnsibleError
    from ansible.module_utils.common.json import Direction, get_module_decoder
    from ansible.plugins.action import ActionBase
    from ansible.utils.display import Display

    decoder = get_module_decoder("legacy", Direction.MODULE_TO_CONTROLLER)

    class Unsupported(AnsibleError, AttributeError):
        """A member the stand-in lacks. An AttributeError as well, so that a probe such as
        `getattr(connection, name, default)` answers as it would for a real connection without
        that member, while a plain read names it."""

    def answered(reply):
        if "error" in reply:
            # The controller's refusal of the ask itself, which the plugin sees as its own error;
            # any other is the link's.
            message = str(reply["error"])
            if message.startswith("\x00plugin:"):
                raise AnsibleError(message[len("\x00plugin:"):])
            raise AnsibleConnectionFailure(message)
        return reply.get("value")

    def text(value, what):
        if isinstance(value, bytes):
            try:
                return value.decode("utf-8")
            except UnicodeDecodeError:
                raise Unsupported("%s that is not UTF-8 is not supported yet" % what)
        return value

    class Link:
        become = None
        has_native_async = False
        always_pipeline_modules = False
        allow_executable = True
        module_implementation_preferences = (".py", "")
        _remote_is_local = False
        # Read by ansible-core's own `_execute_module`, which runs a module shipped as itself
        # (`WANT_JSON`, or one that only prints its JSON) here:
        # no persistent connection stands behind the link.
        socket_path = None

        def __init__(self, ident, shell, play_context, ssh_options=None):
            self._ident = ident
            self._shell = shell
            self._play_context = play_context
            self.transport = play_context.connection
            self.ansible_name = "ansible.builtin.%s" % play_context.connection
            self.cwd = None
            self._options = {
                "remote_addr": play_context.remote_addr,
                "remote_user": play_context.remote_user,
            }
            if play_context.connection == "ssh":
                # What an ssh connection answers for the arguments a plugin hands to its own
                # `ssh`: the host's own, and ansible-core's default for `ssh_args`.
                options = ssh_options or {}
                self._options["ssh_args"] = "-C -o ControlMaster=auto -o ControlPersist=60s"
                self._options["ssh_common_args"] = options.get("ssh_common_args")
                self._options["ssh_extra_args"] = options.get("ssh_extra_args")

        def __getattr__(self, name):
            if name.startswith("__"):
                raise AttributeError(name)
            raise Unsupported("Connection.%s is not supported yet" % name)

        def _ask(self, op, **fields):
            return ask(self._ident, op, **fields)

        def get_option(self, option, hostvars=None):
            if option not in self._options:
                raise Unsupported("the connection option %s is not supported yet" % option)
            return self._options[option]

        def is_pipelining_enabled(self, wrap_async=False):
            # Modules never travel through `exec_command` here: `_execute_module` asks the
            # controller, which runs them its own way.
            return False

        def exec_command(self, cmd, in_data=None, sudoable=True):
            value = answered(self._ask(
                "command",
                cmd=text(cmd, "a command"),
                in_data=text(in_data, "Connection.exec_command input"),
                sudoable=bool(sudoable),
            ))
            if not (
                isinstance(value, dict)
                and isinstance(value.get("rc"), int)
                and isinstance(value.get("stdout"), str)
                and isinstance(value.get("stderr"), str)
            ):
                raise AnsibleConnectionFailure(
                    "the controller answered a command in a shape it should not"
                )
            return value["rc"], value["stdout"].encode(), value["stderr"].encode()

        def put_file(self, in_path, out_path):
            answered(self._ask("put_file", local=in_path, remote=out_path, mode=None))

        def fetch_file(self, in_path, out_path):
            answered(self._ask("fetch_file", remote=in_path, local=out_path))

    def execute_module(self, module_name=None, module_args=None, tmp=None, task_vars=None,
                       persist_files=False, delete_remote_tmp=None, wrap_async=False,
                       ignore_unknown_opts=False):
        # `tmp` and `delete_remote_tmp` are ignored as ansible-core ignores them; `persist_files`
        # and `task_vars` shape the module's upload and interpreter, which the controller owns.
        name = self._task.action if module_name is None else module_name
        if shipped_as_itself(name):
            # Copied with its arguments file and run over the link, by ansible-core's own code.
            return original[0](
                self, module_name, module_args, tmp, task_vars, persist_files,
                delete_remote_tmp, wrap_async, ignore_unknown_opts,
            )
        args = plain(self._task.args if module_args is None else module_args)
        # What `_update_module_args` adds: the name the module's own messages call it by.
        args = dict(args, _ansible_module_name=name)
        if ignore_unknown_opts:
            # The module drops what it does not declare, as it does for the reference.
            args["_ansible_ignore_unknown_opts"] = True
        fields = {"name": name, "args": args, "files": []}
        if wrap_async:
            # Started as a job of the host with the task's `async_val` as its limit, which
            # `gather_facts` sets before each module it runs in parallel: the controller answers
            # with the job's first status, and the plugin polls `async_status` as it would the
            # reference's `async_wrapper`.
            fields["job"] = int(self._task.async_val or 0)
        result = answered(self._connection._ask("module", **fields))
        if not isinstance(result, dict):
            raise AnsibleError("the controller answered the module %s with %r" % (name, result))
        # As `_parse_returned_data` reads a module's output: what the module wrote as a tagged
        # object (its warnings and deprecations) is that object again, and its `exception` is the
        # error summary a plugin such as `gather_facts` reads off a failed result.
        result = json.loads(json.dumps(result), cls=decoder)
        _captured.AnsibleModuleCapturedError.normalize_result_exception(result)
        return result

    def make_tmp_path(self, remote_user=None):
        path = answered(self._connection._ask("tmp", action="make", remote_user=remote_user))
        if not isinstance(path, str) or path.strip("/") == "":
            raise AnsibleError(
                "failed to resolve remote temporary directory: the controller answered %r" % path
            )
        self._cleanup_remote_tmp = True
        self._connection._shell.tmpdir = path
        return path

    def remove_tmp_path(self, tmp_path, force=False):
        if tmp_path is None and self._connection._shell.tmpdir:
            tmp_path = self._connection._shell.tmpdir
        if tmp_path is None or not (force or self._should_remove_tmp_path(tmp_path)):
            return
        reply = self._connection._ask("tmp", action="remove", path=tmp_path)
        if "error" in reply:
            Display().warning("Error deleting remote temporary files (%s)" % reply["error"])
        else:
            self._connection._shell.tmpdir = None

    # A plugin that makes a connection of its own (`synchronize` runs rsync on the controller
    # through a `local` one) gets ansible-core's own methods: only the stand-in carries `_ident`
    # and answers to the controller.
    original = (
        ActionBase._execute_module, ActionBase._make_tmp_path, ActionBase._remove_tmp_path
    )

    def toward_link(shimmed, kept):
        def method(self, *args, **kwargs):
            if isinstance(self._connection, Link):
                return shimmed(self, *args, **kwargs)
            return kept(self, *args, **kwargs)

        return method

    ActionBase._execute_module = toward_link(execute_module, original[0])
    ActionBase._make_tmp_path = toward_link(make_tmp_path, original[1])
    ActionBase._remove_tmp_path = toward_link(remove_tmp_path, original[2])
    SHIM.update(Link=Link, Unsupported=Unsupported)
    return SHIM


def trusted_as_template(value):
    """`value` with each string trusted as a template, as the reference trusts what an inventory
    or a play wrote."""
    from ansible.template import trust_as_template

    if isinstance(value, str):
        return trust_as_template(value)
    if isinstance(value, dict):
        return {k: trusted_as_template(v) for k, v in value.items()}
    if isinstance(value, list):
        return [trusted_as_template(item) for item in value]
    return value


class LazyHostVars(collections.abc.Mapping):
    """`hostvars` for an action plugin, as ansible-core's `HostVars` answers it: a host's
    variables asked of the controller the first time a plugin reads them, and handed out as the
    reference's `HostVarsVars`, which renders a variable when it is read, in that host's context.
    A host no layer above holds a value from a managed host for has its strings trusted as
    templates; any other host's variables are data and never rendered. A host the controller does
    not know is the reference's undefined `hostvars[...]`."""

    def __init__(self, ident, names, loader):
        self._ident = ident
        self._names = list(dict.fromkeys(names))
        self._loader = loader
        self._asked = {}

    def _variables(self, host):
        if not isinstance(host, str):
            return None
        if host not in self._asked:
            from ansible.errors import AnsibleError

            reply = ask(self._ident, "hostvar", host=host)
            if "error" in reply:
                raise AnsibleError(str(reply["error"]))
            value = reply.get("value")
            if value is not None and not (
                isinstance(value, dict)
                and isinstance(value.get("vars"), dict)
                and isinstance(value.get("trusted"), bool)
            ):
                raise AnsibleError(
                    "the controller answered the variables of %s in a shape it should not" % host
                )
            if value is not None and value["trusted"]:
                value = trusted_as_template(value["vars"])
            elif value is not None:
                value = value["vars"]
            self._asked[host] = value
        return self._asked[host]

    def __getitem__(self, host):
        from ansible._internal._templating import _jinja_bits
        from ansible.vars.hostvars import HostVarsVars

        variables = self._variables(host)
        if variables is None:
            return _jinja_bits._undef("hostvars[%r]" % (host,))
        return HostVarsVars(variables, loader=self._loader, host=host)

    def __contains__(self, host):
        return self._variables(host) is not None

    def __iter__(self):
        return iter(self._names)

    def __len__(self):
        return len(self._names)

    def raw_get(self, host):
        # `HostVars.raw_get`: the host's variables, not rendered.
        return self._variables(host)


class PreloadedHostVars(LazyHostVars):
    """`hostvars` for a lookup, whose call cannot ask the controller: every host's variables
    sent with the call, in the shape a `hostvar` ask is answered."""

    def __init__(self, hosts, loader):
        super().__init__(None, list(hosts), loader)
        for host, value in hosts.items():
            self._asked[host] = trusted_as_template(value["vars"]) if value["trusted"] else value["vars"]

    def _variables(self, host):
        return self._asked.get(host) if isinstance(host, str) else None


def result_compat(result, warning_ctx):
    """`TaskExecutor._apply_task_result_compat` of ansible-core 2.19.12: the warnings and
    deprecations a result carries, captured beside the ones raised through `Display`. Written out
    because `ansible.executor.task_executor` imports `ansible.cli`, which exits on a non-blocking
    stderr, as the helper's is under an ssh control master."""
    from ansible.module_utils._internal import _messages
    from ansible.module_utils.datatag import deprecator_from_collection_name
    from ansible.utils.display import Display

    display = Display()
    if warnings := result.get("warnings"):
        if isinstance(warnings, list):
            for warning in warnings:
                if not isinstance(warning, _messages.WarningSummary):
                    warning = _messages.WarningSummary(event=_messages.Event(msg=str(warning)))
                warning_ctx.capture(warning)
        else:
            display.warning(f"Task result `warnings` was {type(warnings)} instead of {list}.")
    if deprecations := result.get("deprecations"):
        if isinstance(deprecations, list):
            for deprecation in deprecations:
                if not isinstance(deprecation, _messages.DeprecationSummary):
                    try:
                        collection_name = deprecation.pop("collection_name", ...)
                        if collection_name is not ...:
                            deprecation.update(
                                deprecator=deprecator_from_collection_name(collection_name)
                            )
                        deprecation = _messages.DeprecationSummary(
                            event=_messages.Event(msg=deprecation.pop("msg")), **deprecation
                        )
                    except Exception as ex:
                        display.error_as_warning(
                            "Task result `deprecations` contained an invalid item.", exception=ex
                        )
                warning_ctx.capture(deprecation)
        else:
            display.warning(
                f"Task result `deprecations` was {type(deprecations)} instead of {list}."
            )


def captured_messages(result, warning_ctx):
    """What `TaskExecutor._execute` leaves of the warnings and deprecations of a task: the ones
    the plugin raised through `Display` and the ones its result carried, each once, in the shape
    a registered result holds them. A deprecation is shown on stderr as the reference's callback
    shows it, since nothing on the controller draws one from a result."""
    from ansible.utils.display import Display

    result.pop("warnings", None)
    result.pop("deprecations", None)
    warnings = [summary_shown(w) for w in warning_ctx.get_warnings()]
    if warnings:
        result["warnings"] = warnings
    deprecations = warning_ctx.get_deprecation_warnings()
    # Written by the controller after the task's warnings, where the reference's callback has them:
    # the text `Display` would print, captured rather than printed now.
    shown = io.StringIO()
    with contextlib.redirect_stderr(shown):
        for summary in deprecations:
            Display()._deprecated(summary)
    if shown.getvalue():
        result["_volant_deprecation_text"] = shown.getvalue()
    if deprecations:
        result["deprecations"] = [summary_shown(d) for d in deprecations]


def handler_for(action):
    """The action plugin `TaskExecutor._get_action_handler` gives a task over a connection the
    helper owns: the module's own plugin when ansible-core has one (`shell`, `copy`, `ping`'s
    `normal`), else the network group's plugin its prefix names, else `normal`."""
    from ansible import constants as C
    from ansible.plugins import loader as plugin_loader

    collection, _, module = action.rpartition(".")
    prefix = module.split("_")[0]
    network = "%s.%s" % (collection, prefix) if collection else prefix
    if plugin_loader.action_loader.has_plugin(action):
        return action
    if prefix in C.NETWORK_GROUP_MODULES and plugin_loader.action_loader.has_plugin(network):
        return network
    return "ansible.legacy.normal"


def owned_connection(link, context, task, task_vars, templar):
    """The connection plugin `link` names, made as `TaskExecutor._get_connection` and
    `_set_connection_options` make it: loaded with the play context, its become plugin set when
    the task becomes (the one `link` names, else the task's method), and the options of the
    connection, its shell and its become plugin read from the task's variables and keywords. It
    is not connected yet."""
    from ansible import constants as C
    from ansible.errors import AnsibleError, AnsibleValueOmittedError
    from ansible.module_utils.parsing.convert_bool import boolean
    from ansible.plugins import loader as plugin_loader

    name = link.get("connection")
    # The play context holds the connection as it was written (`netconf`, not its full name), and
    # the network OS, before the plugin loads: `NetworkConnectionBase` reads both as it starts,
    # and the network action plugins quote the first.
    written = task_vars.get("ansible_connection") or task.connection or name
    context.connection = templar.template(written) if isinstance(written, str) else name
    if task_vars.get("ansible_network_os"):
        context.network_os = templar.template(task_vars["ansible_network_os"])
    connection = plugin_loader.connection_loader.get(
        name, context, new_stdin=None, task_uuid=task._uuid, ansible_playbook_pid=str(os.getppid())
    )
    if not connection:
        raise AnsibleError("the connection plugin '%s' was not found" % name)
    if task_vars.get("ansible_become") is not None:
        becomes = boolean(templar.template(task_vars["ansible_become"]))
    else:
        becomes = task.become
    # A boolean, as `set_task_and_variable_override` leaves it: `network_cli` compares it with `^`.
    context.become = bool(becomes)
    become = None
    if becomes:
        if task_vars.get("ansible_become_method"):
            method = templar.template(task_vars["ansible_become_method"])
        else:
            method = link.get("become") or task.become_method
        become = plugin_loader.become_loader.get(method)
        if not become:
            raise AnsibleError(
                "Invalid become method specified, could not find matching plugin: '%s'. "
                "Use `ansible-doc -t become -l` to list available plugins." % method
            )
    try:
        connection.set_become_plugin(become)
    except AttributeError:
        pass
    if become:
        context.set_become_plugin(become.name)
    context.set_attributes_from_plugin(connection)
    task_keys = task.dump_attrs()
    # The task's own `timeout` is not the connection's, and its `retries` are not either.
    task_keys["timeout"] = context.timeout
    if context.password:
        task_keys["password"] = context.password
    task_keys.pop("retries", None)
    connection.set_options(
        task_keys=task_keys, var_options=connection._resolve_option_variables(task_vars, templar)
    )

    def plugin_options(kind, plugin):
        options = {}
        for key in C.config.get_plugin_vars(kind, plugin._load_name):
            if key in task_vars:
                try:
                    options[key] = templar.template(task_vars[key])
                except AnsibleValueOmittedError:
                    pass
        plugin.set_options(task_keys=task_keys, var_options=options)

    plugin_options("shell", connection._shell)
    if connection.become is not None:
        if context.become_pass:
            task_keys["become_pass"] = context.become_pass
        plugin_options("become", connection.become)
        for option in ("become_user", "become_flags", "become_exe", "become_pass"):
            try:
                setattr(context, option, connection.become.get_option(option))
            except KeyError:
                pass
        context.prompt = connection.become.prompt
    return connection


def persistent(connection, handler, context, task):
    """What `TaskExecutor._get_action_handler_with_module_context` does for a connection that
    keeps a process of its own (`network_cli`, `netconf`, `httpapi`): unless the handler needs no
    connection, ansible-core's `start_connection` runs its stub with this helper's interpreter,
    plugin paths and verbosity, or finds the socket an earlier task left, and the connection
    talks to it. Returns the connection the handler gets and the socket path, `None` for any
    other connection."""
    from ansible import constants as C
    from ansible.executor.task_executor import start_connection
    from ansible.plugins import loader as plugin_loader
    from ansible.utils.display import Display

    keeps = (connection.supports_persistence and C.USE_PERSISTENT_CONNECTIONS)
    if not (keeps or connection.force_persistence):
        return connection, None
    handler_class = plugin_loader.action_loader.get(handler, class_only=True)
    if not getattr(handler_class, "_requires_connection", True):
        local = plugin_loader.connection_loader.get("local", context, os.devnull)
        local.set_options()
        return local, None
    display = Display()
    context.timeout = connection.get_option("persistent_command_timeout")
    display.vvvv("attempting to start connection", host=context.remote_addr)
    display.vvvv("using connection plugin %s" % connection.transport, host=context.remote_addr)
    socket_path = start_connection(context, connection.get_options(), task._uuid)
    display.vvvv("local domain socket path is %s" % socket_path, host=context.remote_addr)
    connection._socket_path = socket_path
    return connection, socket_path


def reset_persistent(paths):
    """Ends the persistent connection processes behind `paths`, as `meta: reset_connection` and
    the strategy's `cleanup` at the end of a play end them; one already gone is no error."""
    from ansible.module_utils.connection import Connection, ConnectionError

    for path in paths:
        try:
            Connection(path).reset()
        except ConnectionError:
            pass
    return len(paths)


@contextlib.contextmanager
def displayed(events):
    """What ansible-core's `Display` says while an owned link's action runs, appended to `events`
    in order as `{level, host, text}` rather than printed: a `verbose` line at its level (`vvv` is
    3), a plain `display` at 0, and a warning or a deprecation as `warning` and `deprecation`,
    which still reach the result's own `warnings` and `deprecations` as well. Every level is kept;
    the controller prints what its verbosity shows."""
    from ansible.utils.display import Display

    kept = Display.verbose, Display.display, Display.warning, Display.deprecated

    def verbose(self, msg, host=None, caplevel=2):
        events.append({"level": caplevel + 1, "host": host, "text": str(msg)})

    def display(self, msg, *args, **kwargs):
        # A line meant for the log file alone, as a persistent connection's `log` message is.
        if not kwargs.get("log_only"):
            events.append({"level": 0, "host": None, "text": str(msg)})

    def warning(self, msg, *args, **kwargs):
        events.append({"level": "warning", "host": None, "text": str(msg)})
        return kept[2](self, msg, *args, **kwargs)

    def deprecated(self, msg, *args, **kwargs):
        events.append({"level": "deprecation", "host": None, "text": str(msg)})
        return kept[3](self, msg, *args, **kwargs)

    Display.verbose, Display.display, Display.warning, Display.deprecated = (
        verbose, display, warning, deprecated
    )
    try:
        yield
    finally:
        Display.verbose, Display.display, Display.warning, Display.deprecated = kept


# The variables that hold a connection's or an escalation's secret.
SECRET_VAR = re.compile(r"^ansible_(?:\w+_)?(?:pass|password|passwd|secret|token|passphrase)$")


def without_secrets(value, task_vars, context):
    """`value` with every secret the task's variables or play context hold written `********`,
    in every string it holds, however deep."""
    secrets = {
        v for k, v in task_vars.items()
        if isinstance(k, str) and SECRET_VAR.match(k) and isinstance(v, str) and v
    }
    secrets.update(
        v for v in (getattr(context, "password", None), getattr(context, "become_pass", None))
        if isinstance(v, str) and v
    )
    ordered = sorted(secrets, key=len, reverse=True)

    def scrub(item):
        if isinstance(item, str):
            for secret in ordered:
                item = item.replace(secret, "********")
            return item
        if isinstance(item, dict):
            return {scrub(k): scrub(v) for k, v in item.items()}
        if isinstance(item, list):
            return [scrub(v) for v in item]
        return item

    return scrub(value) if ordered else value


def run_action(ident, request):
    """One task's action plugin, run as ansible-core's `TaskExecutor` runs it: the plugin
    `action_loader` finds for `name`, given the task, the play context and the stand-in
    connection, then `run(task_vars=…)` and `cleanup()`. A failure of the plugin is its task's
    failed result, shaped by ansible-core; only a request it cannot even start is an error.

    With `link.kind` `owned` the connection is a real connection plugin the helper makes
    ([`owned_connection`]), connected before the plugin runs and closed once after it, on every
    path; what `Display` says comes back in the value as `_volant_display`, and no secret of the
    task's is in the value. A connection that keeps a process of its own starts it first
    ([`persistent`]), and its socket comes back as `_volant_socket`."""
    from ansible._internal import _display_utils
    from ansible._internal._errors import _error_utils
    from ansible.errors import AnsibleError, AnsibleTaskError
    from ansible.parsing.dataloader import DataLoader
    from ansible.playbook.block import Block
    from ansible.playbook.play import Play
    from ansible.playbook.play_context import PlayContext
    from ansible.playbook.task import Task
    from ansible.plugins import loader as plugin_loader
    from ansible.template import Templar

    loaders()
    parts = shim()
    link = request.get("link") or {}
    owned = link.get("kind") == "owned"
    events = []
    socket_path = None
    if owned and "verbosity" in link:
        # Plugins that test the verbosity themselves see the run's.
        from ansible.utils.display import Display

        Display().verbosity = int(link["verbosity"])
    name = request["name"]
    fields = dict(request.get("task") or {})
    written = fields.pop("_volant_action", None) or name
    # The action is `name`, and the controller has merged its module defaults into the arguments:
    # the task's `module_defaults` are for the modules the plugin picks itself.
    for keyword in ("action", "local_action"):
        if keyword in fields:
            message = "the task keyword %s is not supported for an action plugin yet" % keyword
            return {"error": {"kind": "plugin", "message": message}}
    if owned and written == "ansible.legacy.setup":
        # The play's own `Gathering Facts`, named so by the controller, which ansible-core runs as
        # its `gather_facts` plugin: that plugin picks a network host's facts module from its
        # `ansible_network_os`, and `setup` for any other host.
        written = name = "ansible.legacy.gather_facts"
    if owned and name in ("normal", "ansible.builtin.normal", "ansible.legacy.normal"):
        name = handler_for(written)
    if not CALL_NAME.match(name) or not plugin_loader.action_loader.has_plugin(name):
        return {"error": {"kind": "not_found", "message": "the handler '%s' was not found" % name}}
    task_vars = dict(request.get("task_vars") or {})
    search_path = task_vars.pop("_volant_search_path", None)
    ssh_options = task_vars.pop("_volant_ssh_options", None) or {}
    # What the plugin says through `Display` is kept for its result, as `TaskExecutor._execute`
    # keeps it, rather than printed here.
    shown = displayed(events) if owned else contextlib.nullcontext()
    with _display_utils.DeferredWarningContext(variables=task_vars) as warning_ctx, shown:
        task = None
        context = None
        try:
            context = PlayContext()
            for key, value in (request.get("play_context") or {}).items():
                if key not in PlayContext.fattributes:
                    raise AnsibleError("the play context field %s is not supported yet" % key)
                setattr(context, key, value)
            # A connection takes its shell from the play context, which `ansible_shell_type`
            # sets as `PlayContext.set_task_and_variable_override` does.
            if task_vars.get("ansible_shell_type"):
                context.shell = task_vars["ansible_shell_type"]
            shell_type = task_vars.get("ansible_shell_type") or "sh"
            shell = plugin_loader.shell_loader.get(shell_type)
            if shell is None:
                raise AnsibleError("the shell plugin %s is not supported yet" % shell_type)
            shell.set_options(var_options=task_vars)
            # What ansible-core's own `_execute_module` reads to build a module that runs here.
            task_vars.setdefault("ansible_playbook_python", sys.executable)
            groups = task_vars.get("groups")
            names = groups.get("all", []) if isinstance(groups, dict) else []
            loader = DataLoader()
            anchor = task_vars.get("playbook_dir")
            if isinstance(anchor, str) and anchor:
                loader.set_basedir(anchor)
            task_vars["hostvars"] = LazyHostVars(ident, names, loader)
            templar = Templar(loader=loader, variables=task_vars)
            fields["action"] = written
            # A play of its own, whose action groups loading the `module_defaults` fills.
            task = Task.load(fields, block=Block(play=Play()), loader=loader)
            task.post_validate(templar=templar._engine)
            if search_path:
                # The role and file directories the controller compiled the task in, which the
                # plugin's `_find_needle` reads off the task as the reference's role-bound task gives.
                task.get_search_path = lambda: list(search_path)
            if owned:
                # A delegated task connects to the delegate, with the delegate's variables, as
                # `TaskExecutor._execute` connects it.
                connection_vars = task_vars
                if task.delegate_to:
                    delegated = task_vars.get("ansible_delegated_vars") or {}
                    connection_vars = delegated.get(task.delegate_to) or {}
                connection = owned_connection(link, context, task, connection_vars, templar)
            else:
                connection = parts["Link"](ident, shell, context, ssh_options)
            try:
                if owned:
                    connection, socket_path = persistent(connection, name, context, task)
                if owned and socket_path is None:
                    # Connected before the plugin runs, so a refusal is the host's, unreachable. A
                    # persistent process that cannot start fails the task instead, as it does there.
                    connection._connect()
                action = plugin_loader.action_loader.get(
                    name,
                    task=task,
                    connection=connection,
                    play_context=context,
                    loader=loader,
                    templar=templar,
                    shared_loader_obj=plugin_loader,
                )
                if action is None:
                    raise AnsibleError("the handler '%s' was not found" % name)
                try:
                    result = action.run(task_vars=task_vars)
                    if isinstance(result, dict):
                        # What `TaskExecutor._execute` fills in on a result that came back, and a
                        # key only this helper may set, which the plugin's own data cannot carry.
                        for key in [key for key in result if str(key).startswith("_volant_")]:
                            del result[key]
                        result.setdefault("changed", False)
                        result_compat(result, warning_ctx)
                        if result.get("unreachable") is True and not result.get("failed"):
                            # Returned rather than raised: no error block, and `failed` registered.
                            result["_volant_returned"] = True
                        # An error that gathers others (`gather_facts`'s failed modules) keeps its
                        # shape for the controller's error block, which frames each one.
                        event = getattr(result.get("exception"), "event", None)
                        if event is not None and getattr(event, "events", None):
                            result["_volant_cause"] = {
                                "event": {
                                    "msg": event.msg,
                                    "events": [{"msg": e.msg} for e in event.events],
                                }
                            }
                finally:
                    action.cleanup()
            finally:
                if owned:
                    connection.close()
        except Exception as failure:
            # As `TaskExecutor._execute` shapes it: what the exception contributes comes first, so a
            # connection failure is unreachable, `AnsibleActionSkip` skipped, `AnsibleActionFail`
            # its own result, and anything else a failure under the task's error.
            try:
                raise AnsibleTaskError(obj=task.get_ds() if task is not None else None) from failure
            except AnsibleTaskError as wrapped:
                result = _error_utils.result_dict_from_exception(wrapped, accept_result_contribution=True)
                result.setdefault("changed", False)
                # Raised, so the result never goes through the part of `_execute` that adds `failed`.
                from ansible.errors import AnsibleActionFail, AnsibleActionSkip

                if isinstance(failure, AnsibleActionSkip):
                    result["_volant_raised_skip"] = True
                elif isinstance(failure, AnsibleActionFail):
                    result["_volant_raised_fail"] = True
            # The summary object carries the traceback text only when ansible-core is set to keep it.
            summary = result.pop("exception", None)
            traceback = getattr(getattr(summary, "event", None), "formatted_traceback", None)
            if traceback:
                result["exception"] = traceback
    if isinstance(result, dict):
        captured_messages(result, warning_ctx)
    value = plain(result)
    if owned:
        if isinstance(value, dict):
            value["_volant_display"] = events
            if socket_path:
                value["_volant_socket"] = socket_path
        value = without_secrets(value, task_vars, context)
    return {"value": value}


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
    # They are private copies of the pipes, and descriptors 0 and 1 point elsewhere: a plugin's
    # subprocess (`lookup('pipe', 'cat')`) inherits those, and must not read the next request or
    # write into a frame.
    frames = os.fdopen(os.dup(1), "wb")
    requests = os.fdopen(os.dup(0), "rb")
    null = os.open(os.devnull, os.O_RDONLY)
    os.dup2(null, 0)
    os.close(null)
    sys.stdin = open(os.devnull)
    os.dup2(2, 1)
    sys.stdout = sys.stderr
    # `python -c` puts the working directory first on the path, which ansible-playbook, a script,
    # never does: an `ansible` directory where the run starts is not the ansible-core to build
    # with, and the working directory is no file a kept union should depend on.
    sys.path[:] = [entry for entry in sys.path if entry]
    PIPES.update(requests=requests, frames=frames)
    while True:
        request = read_frame(requests)
        if request is None:
            return
        if "call" in request or "action" in request:
            try:
                if "action" in request:
                    answer = run_action(request.get("id"), request["action"])
                else:
                    answer = call(request["call"])
            except Exception as failure:
                answer = {"error": {"kind": "plugin", "message": str(failure)}}
            answer["id"] = request.get("id")
            write_frame(frames, answer)
            continue
        try:
            if "plugin_dirs" in request:
                answer = plugin_dirs(request["plugin_dirs"], request.get("module_path", ()))
            elif "resolve" in request:
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
    shaped = plain({"a": (1, {2}), "b": b"x", "c": datetime.date(2020, 1, 2), 3: iter("y")})
    assert shaped == {"a": [1, [2]], "b": "x", "c": "2020-01-02", "3": ["y"]}, shaped
    try:
        plain(b"\xff")
    except RuntimeError as bytes_:
        assert "not UTF-8" in str(bytes_), bytes_
    else:
        raise AssertionError("bytes that are not UTF-8 were turned into text")
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
        # A package directory with no `__init__.py`, which module_common.py fills with an empty
        # one: the directory is what an added `__init__.py` would change.
        namespace = os.path.join(home, "plugins", "module_utils", "_mh")
        os.makedirs(namespace)
        TRACKED.clear()
        assert copies(
            {"ansible_collections/ns/c/plugins/module_utils/_mh/__init__.py": b""}, site, homes
        ), TRACKED
        assert sorted(TRACKED) == [namespace], TRACKED
        for odd in (
            # The same empty init for a directory that is not there.
            {"ansible_collections/ns/c/plugins/module_utils/gone/__init__.py": b""},
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
