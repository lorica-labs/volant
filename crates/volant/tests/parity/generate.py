# SPDX-License-Identifier: GPL-3.0-or-later
"""Lists every name the reference ansible-core and the pinned collection corpus know.

Run by `just parity-reference` with the ansible-core tool interpreter, on a machine with that
tool installed. Writes reference.json next to this file. The Rust test tests/parity.rs reads it.
Argument: the directory holding the `ansible` package's `ansible_collections`.
"""
import json
import os
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


def traits(path):
    out = []
    modules = os.path.join(path, "plugins", "modules")
    if os.path.isdir(modules) and any(f.endswith(".ps1") for f in os.listdir(modules)):
        out.append("windows")
    if any(os.path.isdir(os.path.join(path, "plugins", d)) and
           any(f.endswith(".py") and f != "__init__.py"
               for f in os.listdir(os.path.join(path, "plugins", d)))
           for d in ("cliconf", "terminal", "httpapi", "netconf")):
        out.append("network")
    return out


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
        entry = {"version": manifest["collection_info"]["version"], "traits": traits(path)}
        for t in COLLECTION_TYPES:
            entry[t] = doc_list(t, fqcn)
        entry["action"] = sorted(f"{fqcn}.{n}" for n in
                                 action_files(os.path.join(path, "plugins", "action")))
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
