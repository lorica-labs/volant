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
import datetime
import importlib.util
import io
import itertools
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


def actions(names):
    """Whether each name a `module_defaults` key spells resolves to an action or a module."""
    loaders()
    return {name: resolve_action(name) is not None for name in names}


def diff_text(diffs):
    """The text `ansible-playbook` prints for a result's `diff`: ansible-core's own
    `CallbackBase._get_diff`, uncoloured (the controller paints it)."""
    from ansible.plugins.callback import CallbackBase
    from ansible.utils import color

    color.ANSIBLE_COLOR = False
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
    "lookup": "lookup(volant_name__, *volant_args__, **volant_kwargs__)",
}


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
    if isinstance(value, collections.abc.Mapping):
        return {
            str.__str__(k) if isinstance(k, str) else str(k): plain(v) for k, v in value.items()
        }
    if isinstance(value, (collections.abc.Iterable)):
        return [plain(item) for item in value]
    raise RuntimeError("a value of type %s cannot be returned" % type(value).__name__)


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


def inventory(path):
    """One collection inventory plugin reading `path`, as the reference's `auto` plugin runs it,
    inside the same `_InventoryDataWrapper` the inventory manager gives it (so a plugin's trust
    and origins are the reference's) and with its cache: the groups, children, hosts and host
    variables it made as plain JSON, and the lines it said. A failure is answered as data, so
    the warnings said before it are kept."""
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
        auto = inventory_loader.get("auto")
        origin = Origin(description="<inventory plugin 'auto' with source %r>" % path)
        wrapper = _InventoryDataWrapper(manager._inventory, target_plugin=auto, origin=origin)
        auto.parse(wrapper, loader, path, cache=True)
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


def call(request):
    """One filter, test or lookup, run through ansible-core's own Jinja environment so its
    argument handling, error text and `errors`/`wantlist` shaping are the reference's."""
    from ansible.parsing.dataloader import DataLoader
    from ansible.template import Templar, trust_as_template

    loaders()
    kind, name = request["kind"], request["name"]
    if kind == "inventory":
        return {"value": inventory(request["args"][0])}
    if kind == "diff":
        return {"value": diff_text(request["args"][0])}
    if kind == "action_groups":
        return {"value": action_groups(request["args"])}
    if kind == "actions":
        return {"value": actions(request["args"])}
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
        variables = request.get("vars") or {}
        # The loader a lookup resolves relative names and runs commands through, anchored where
        # the reference anchors it: the playbook's directory, else the first search path entry.
        loader = DataLoader()
        anchor = variables.get("playbook_dir") or next(iter(variables.get("ansible_search_path") or []), None)
        if isinstance(anchor, str) and anchor:
            loader.set_basedir(anchor)
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
            return {"value": None}
        value = templar.evaluate_expression(
            trust_as_template(expression),
            local_variables={
                "volant_args__": args,
                "volant_kwargs__": request["kwargs"],
                "volant_name__": name,
            },
        )
        return {"value": plain(value)}
    except Exception as failure:
        # Named rather than imported: the class lives in ansible-core's private templating
        # package, and its name is what 2.19 keeps stable.
        if type(failure).__name__ == "AnsibleTemplatePluginNotFoundError":
            return {"error": missing}
        return {"error": {"kind": "plugin", "message": str(failure)}}


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
# The results of the jobs `wrap_async` started, by (request id, job id), until polled.
JOBS = {}
JOB_IDS = itertools.count(1)


def shim():
    """The stand-in connection an action plugin is given, and `ActionBase` turned toward it, made
    once ansible-core is imported. What a plugin asks of the host goes to the controller, which
    runs it over the host's own link; a member the corpus's plugins do not use is refused by name.
    """
    if SHIM:
        return SHIM
    from ansible.errors import AnsibleConnectionFailure, AnsibleError
    from ansible.plugins.action import ActionBase
    from ansible.utils.display import Display

    class Unsupported(AnsibleError, AttributeError):
        """A member the stand-in lacks. An AttributeError as well, so that a probe such as
        `getattr(connection, name, default)` answers as it would for a real connection without
        that member, while a plain read names it."""

    def answered(reply):
        if "error" in reply:
            raise AnsibleConnectionFailure(str(reply["error"]))
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

        def __init__(self, ident, shell, play_context):
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
        args = plain(self._task.args if module_args is None else module_args)
        ident = self._connection._ident
        if name.rsplit(".", 1)[-1] == "async_status" and not wrap_async:
            return poll(ident, args)
        # What `_update_module_args` adds: the name the module's own messages call it by.
        args = dict(args, _ansible_module_name=name)
        if ignore_unknown_opts:
            # The module drops what it does not declare, as it does for the reference.
            args["_ansible_ignore_unknown_opts"] = True
        started = time.monotonic()
        result = answered(self._connection._ask("module", name=name, args=args, files=[]))
        if not isinstance(result, dict):
            raise AnsibleError("the controller answered the module %s with %r" % (name, result))
        if wrap_async:
            # The module runs to its end before the job is handed back, so parallel
            # jobs run one after the other, and the async limit is judged afterwards: a module
            # that ran past it is replaced by async_wrapper's timeout result.
            limit = int(self._task.async_val or 0)
            if limit and time.monotonic() - started > limit:
                result = {"msg": "Timeout exceeded", "failed": True}
            jid = "j%s.%d" % (ident, next(JOB_IDS))
            JOBS[(ident, jid)] = result
            return {
                "started": 1,
                "finished": 0,
                "ansible_job_id": jid,
                "results_file": "/volant-async/%s" % jid,
                "_ansible_suppress_tmpdir_delete": True,
            }
        return result

    def poll(ident, args):
        jid = args.get("jid")
        mode = args.get("mode", "status")
        if mode not in ("status", "cleanup"):
            raise Unsupported("async_status mode %s is not supported yet" % mode)
        result = JOBS.get((ident, jid))
        if result is None:
            return {
                "failed": True,
                "msg": "could not find job",
                "ansible_job_id": jid,
                "started": 1,
                "finished": 1,
            }
        if mode == "cleanup":
            del JOBS[(ident, jid)]
            return {"ansible_job_id": jid, "erased": "/volant-async/%s" % jid}
        return dict(result, started=1, finished=1, ansible_job_id=jid)

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

    ActionBase._execute_module = execute_module
    ActionBase._make_tmp_path = make_tmp_path
    ActionBase._remove_tmp_path = remove_tmp_path
    SHIM.update(Link=Link, Unsupported=Unsupported)
    return SHIM


class LazyHostVars(collections.abc.Mapping):
    """`hostvars` for an action plugin: the hosts the controller sent, and any other one asked
    of the controller the first time a plugin reads it."""

    def __init__(self, ident, known, names):
        self._ident = ident
        self._known = dict(known)
        self._names = list(dict.fromkeys(list(self._known) + list(names)))

    def __getitem__(self, host):
        if host not in self._known:
            reply = ask(self._ident, "hostvar", host=host)
            if "error" in reply:
                from ansible.errors import AnsibleError

                raise AnsibleError(str(reply["error"]))
            if reply.get("value") is None:
                raise KeyError(host)
            self._known[host] = reply["value"]
        return self._known[host]

    def __iter__(self):
        return iter(self._names)

    def __len__(self):
        return len(self._names)


def run_action(ident, request):
    """One task's action plugin, run as ansible-core's `TaskExecutor` runs it: the plugin
    `action_loader` finds for `name`, given the task, the play context and the stand-in
    connection, then `run(task_vars=…)` and `cleanup()`. A failure of the plugin is its task's
    failed result, shaped by ansible-core; only a request it cannot even start is an error."""
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
    name = request["name"]
    fields = dict(request.get("task") or {})
    # The action is `name`, and the controller has merged the module defaults into the arguments.
    for keyword in ("action", "local_action", "module_defaults"):
        if keyword in fields:
            message = "the task keyword %s is not supported for an action plugin yet" % keyword
            return {"error": {"kind": "plugin", "message": message}}
    if not CALL_NAME.match(name) or not plugin_loader.action_loader.has_plugin(name):
        return {"error": {"kind": "not_found", "message": "the handler '%s' was not found" % name}}
    task_vars = dict(request.get("task_vars") or {})
    search_path = task_vars.pop("_volant_search_path", None)
    task = None
    try:
        context = PlayContext()
        for key, value in (request.get("play_context") or {}).items():
            if key not in PlayContext.fattributes:
                raise AnsibleError("the play context field %s is not supported yet" % key)
            setattr(context, key, value)
        shell_type = task_vars.get("ansible_shell_type") or "sh"
        shell = plugin_loader.shell_loader.get(shell_type)
        if shell is None:
            raise AnsibleError("the shell plugin %s is not supported yet" % shell_type)
        shell.set_options(var_options=task_vars)
        groups = task_vars.get("groups")
        names = groups.get("all", []) if isinstance(groups, dict) else []
        known = task_vars.get("hostvars")
        task_vars["hostvars"] = LazyHostVars(ident, known if isinstance(known, dict) else {}, names)
        loader = DataLoader()
        anchor = task_vars.get("playbook_dir")
        if isinstance(anchor, str) and anchor:
            loader.set_basedir(anchor)
        templar = Templar(loader=loader, variables=task_vars)
        fields["action"] = name
        # A play of its own: `post_validate` reads the play's action groups, which hold nothing
        # here since no module_defaults reach the plugin.
        task = Task.load(fields, block=Block(play=Play()), loader=loader)
        task.post_validate(templar=templar._engine)
        if search_path:
            # The role and file directories the controller compiled the task in, which the
            # plugin's `_find_needle` reads off the task as the reference's role-bound task gives.
            task.get_search_path = lambda: list(search_path)
        action = plugin_loader.action_loader.get(
            name,
            task=task,
            connection=parts["Link"](ident, shell, context),
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
                # What `TaskExecutor._execute` fills in on a result that came back, and a key
                # only this helper may set, which the plugin's own data cannot carry.
                for key in [key for key in result if str(key).startswith("_volant_")]:
                    del result[key]
                result.setdefault("changed", False)
        finally:
            action.cleanup()
            for key in [key for key in JOBS if key[0] == ident]:
                del JOBS[key]
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
            from ansible.errors import AnsibleActionSkip

            if isinstance(failure, AnsibleActionSkip):
                result["_volant_raised_skip"] = True
        # The summary object carries the traceback text only when ansible-core is set to keep it.
        summary = result.pop("exception", None)
        traceback = getattr(getattr(summary, "event", None), "formatted_traceback", None)
        if traceback:
            result["exception"] = traceback
    return {"value": plain(result)}


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
