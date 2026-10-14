# SPDX-License-Identifier: GPL-3.0-or-later
"""Lists every name the reference ansible-core and the pinned collection corpus know.

Run by `just parity-reference` with the ansible-core tool interpreter, on a machine with that
tool installed. Writes reference.json next to this file. The Rust test tests/parity.rs reads it.
Argument: the directory holding the `ansible` package's `ansible_collections`.
"""
import json
import os
import re
import subprocess
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
CORE = open(os.path.join(HERE, "..", "golden", "ANSIBLE_VERSION"), encoding="utf-8").read().strip()
PACKAGE = open(os.path.join(HERE, "ANSIBLE_PACKAGE"), encoding="utf-8").read().strip()
CORPUS = os.path.abspath(sys.argv[1])
os.environ["ANSIBLE_COLLECTIONS_PATH"] = CORPUS
os.environ["ANSIBLE_COLLECTIONS_SCAN_SYS_PATH"] = "False"

import ansible  # noqa: E402

if ansible.__version__ != CORE:
    sys.exit(f"ansible-core {ansible.__version__} is not the reference {CORE}")
if not os.path.isdir(os.path.join(CORPUS, f"ansible-{PACKAGE}.dist-info")):
    sys.exit(f"{CORPUS} does not hold the pinned ansible {PACKAGE}")

import yaml  # noqa: E402
from ansible import constants as C  # noqa: E402
from ansible.cli.playbook import PlaybookCLI  # noqa: E402
from ansible.playbook.block import Block  # noqa: E402
from ansible.playbook.handler import Handler  # noqa: E402
from ansible.playbook.loop_control import LoopControl  # noqa: E402
from ansible.playbook.play import Play  # noqa: E402
from ansible.playbook.task import Task  # noqa: E402

ANSIBLE_DOC = os.path.join(os.path.dirname(sys.executable), "ansible-doc")
CORE_DIR = os.path.dirname(ansible.__file__)
PLUGIN_TYPES = ["module", "filter", "test", "lookup", "become", "connection", "inventory",
                "strategy", "shell", "vars", "cache", "callback"]
COLLECTION_TYPES = ["module", "filter", "test", "lookup"]


def spellings(cls):
    """The keywords a playbook can write for one class: each attribute's alias if it has one.

    `loop_with` is internal (set from a `with_*` key) and has no spelling of its own.
    """
    out = set()
    for name, attr in cls.fattributes.items():
        if name == "loop_with":
            continue
        out.add(getattr(attr, "alias", None) or name)
    return out


def doc_list(ptype, collection):
    """`ansible-doc -t <type> -l -j <collection>`: the names the reference itself lists."""
    run = subprocess.run([ANSIBLE_DOC, "-t", ptype, "-l", "-j", collection],
                         capture_output=True, text=True, check=True)
    return sorted(json.loads(run.stdout or "{}"))


def action_files(directory):
    """Action plugins are files, one per name; ansible-doc has no `action` type."""
    if not os.path.isdir(directory):
        return []
    return sorted(f[:-3] for f in os.listdir(directory)
                  if f.endswith(".py") and f != "__init__.py")


def collections():
    root = os.path.join(CORPUS, "ansible_collections")
    for ns in sorted(os.listdir(root)):
        if not os.path.isdir(os.path.join(root, ns)) or ns.endswith(".dist-info"):
            continue
        for coll in sorted(os.listdir(os.path.join(root, ns))):
            path = os.path.join(root, ns, coll)
            if os.path.isfile(os.path.join(path, "MANIFEST.json")):
                yield f"{ns}.{coll}", path


# A plugin that talks to a device over a persistent connection (network_cli, httpapi, netconf)
# rather than running on the host it is sent to.
PERSISTENT = re.compile(
    r"\._socket_path|module_utils\.connection import|get_connection\((self\.)?_?module"
    r"|ActionNetworkModule")


def sources(directory):
    """Plugin name -> source file, for every `.py` under `directory`, subdirectories included."""
    out = {}
    for root, _, files in os.walk(directory):
        for f in files:
            if f.endswith(".py") and f != "__init__.py":
                out.setdefault(f[:-3], os.path.join(root, f))
    return out


def persistent(path):
    if path is None:
        return False
    with open(path, encoding="utf-8", errors="replace") as f:
        return bool(PERSISTENT.search(f.read()))


def routing(path, fqcn):
    """meta/runtime.yml: module -> the action plugin it runs (`plugin_routing.action` redirects
    and `plugin_routing.modules.<m>.action_plugin`), as the reference's task executor reads it."""
    try:
        with open(os.path.join(path, "meta", "runtime.yml"), encoding="utf-8") as f:
            runtime = yaml.safe_load(f) or {}
    except OSError:
        return {}
    routes = runtime.get("plugin_routing") or {}
    out = {}
    for kind, key in (("action", "redirect"), ("modules", "action_plugin")):
        for name, entry in (routes.get(kind) or {}).items():
            if isinstance(entry, dict) and entry.get(key):
                out[f"{fqcn}.{name}"] = entry[key]
    return out


def imports_persistent(path, fqcn, module_utils, seen=None):
    """Whether a module, or a module_utils file of its own collection it imports, directly or
    through other module_utils files, opens a persistent connection."""
    seen = set() if seen is None else seen
    if path is None or path in seen:
        return False
    seen.add(path)
    with open(path, encoding="utf-8", errors="replace") as f:
        text = f.read()
    if PERSISTENT.search(text):
        return True
    prefix = f"ansible_collections.{fqcn}.plugins.module_utils."
    names = set(re.findall(re.escape(prefix) + r"([\w.]+)", text))
    names |= set(re.findall(r"from \.+([\w.]*) import", text))
    return any(imports_persistent(module_utils.get(n.split(".")[-1]), fqcn, module_utils, seen)
               for n in names if n)


def platform(fqcn, path, modules, actions):
    """What each module and action plugin of one collection needs, beyond ssh or local.

    Returns (module -> action plugin it runs through, name -> reason it waits for later).
    """
    module_dir = os.path.join(path, "plugins", "modules")
    windows = os.path.isdir(module_dir) and any(
        f.endswith(".ps1") for _, _, fs in os.walk(module_dir) for f in fs)
    module_src = sources(module_dir)
    module_utils = sources(os.path.join(path, "plugins", "module_utils"))
    action_src = {f"{fqcn}.{n}": p for n, p in
                  sources(os.path.join(path, "plugins", "action")).items()}
    routes = routing(path, fqcn)
    module_action = {}
    later = {}
    for a in actions:
        if windows:
            later[a] = "Windows"
        elif persistent(action_src.get(a)):
            later[a] = "network connection"
    for m in modules:
        short_name = m.rsplit(".", 1)[1]
        # The reference's order: the module's own routing, an action plugin of its name, then
        # the network group action of its prefix (`ios_acls` -> `cisco.ios.ios`).
        group = f"{fqcn}.{short_name.split('_')[0]}"
        action = routes.get(m) or (m if m in actions else None) or (
            group if short_name.split("_")[0] in C.NETWORK_GROUP_MODULES and group in actions
            else None)
        if action:
            module_action[m] = action
        if windows:
            later[m] = "Windows"
        elif persistent(action_src.get(action)) or                 imports_persistent(module_src.get(short_name), fqcn, module_utils):
            later[m] = "network connection"
    return module_action, later


def short(names, prefix="ansible.builtin."):
    return sorted(n[len(prefix):] if n.startswith(prefix) else n for n in names)


def main():
    builtin = {t: short(doc_list(t, "ansible.builtin")) for t in PLUGIN_TYPES}
    builtin["action"] = action_files(os.path.join(CORE_DIR, "plugins", "action"))

    task = spellings(Task) | {"local_action"} | {f"with_{l}" for l in builtin["lookup"]}
    keywords = {
        "play": sorted(spellings(Play)),
        "block": sorted(spellings(Block)),
        "task": sorted(task),
        "handler": sorted(spellings(Handler) | task),
        "loop_control": sorted(spellings(LoopControl)),
    }

    meta_doc = yaml.safe_load(
        open(os.path.join(CORE_DIR, "modules", "meta.py"), encoding="utf-8").read()
        .split('DOCUMENTATION = r"""', 1)[1].split('"""', 1)[0])
    meta = sorted(meta_doc["options"]["free_form"]["choices"])

    cli = PlaybookCLI(["ansible-playbook", "site.yml"])
    cli.init_parser()
    options = sorted(
        ({"flags": sorted(a.option_strings, key=len), "takes_value": a.nargs != 0}
         for a in cli.parser._actions if a.option_strings),
        key=lambda o: o["flags"][-1])

    corpus = {}
    for fqcn, path in collections():
        manifest = json.load(open(os.path.join(path, "MANIFEST.json"), encoding="utf-8"))
        entry = {"version": manifest["collection_info"]["version"]}
        for t in COLLECTION_TYPES:
            entry[t] = doc_list(t, fqcn)
        entry["action"] = sorted(f"{fqcn}.{n}" for n in
                                 action_files(os.path.join(path, "plugins", "action")))
        entry["module_action"], entry["later"] = platform(
            fqcn, path, entry["module"], entry["action"])
        corpus[fqcn] = entry
    if not corpus:
        sys.exit(f"no collection under {CORPUS}/ansible_collections")

    out = {"ansible_core": CORE, "ansible_package": PACKAGE, "keywords": keywords,
           "meta": meta, "cli": options, "yaml_tags": ["!unsafe", "!vault"],
           "builtin": builtin, "collections": corpus}
    with open(os.path.join(HERE, "reference.json"), "w", encoding="utf-8", newline="\n") as f:
        json.dump(out, f, indent=1, sort_keys=True)
        f.write("\n")


main()
