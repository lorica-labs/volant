# SPDX-License-Identifier: GPL-3.0-or-later
"""The `inventory` kind: inventory.ini read by `ansible-inventory`, every line of patterns.txt
resolved by `ansible --list-hosts`, and the homonym fixture, recorded in expected_inventory.json."""
import json
import os
import shutil
import subprocess
import sys
import tempfile

import yaml

from . import GOLDEN
from .redact import TMP
from .run import Failed, run, write_json

HOMONYM_WARNING = "Found both group and host with same name"
# `--toml` needs this package, which ansible-core does not bring: it is installed in a directory
# of its own and put on the path of the runs that print TOML.
TOMLI_W = "tomli-w==1.2.0"


def _list_hosts(inventory, pattern):
    result = run(["ansible", "-i", inventory, pattern, "--list-hosts"])
    hosts = [h.strip() for h in result.stdout.splitlines()[1:] if h.strip()]
    # The homonym warning alone, not any warning: an unmatched term or an empty result warn too,
    # which the engine keeps in `unmatched`, not `warnings`.
    return {"hosts": hosts, "warning": HOMONYM_WARNING in result.stderr}


def _sources():
    """Every case of inventories/cases.yml, read in a copy of the directory (plus the empty
    directory git cannot hold): rc, stderr, the --list dump, its --yaml, --toml and --export forms and the
    order --list-hosts gives."""
    with open(os.path.join(GOLDEN, "inventories", "cases.yml"), encoding="utf-8") as f:
        cases = yaml.safe_load(f)
    recorded = {}
    with tempfile.TemporaryDirectory() as tmp, tempfile.TemporaryDirectory() as lib:
        subprocess.run(
            ["uv", "pip", "install", "--quiet", "--python", sys.executable, "--target", lib, TOMLI_W],
            check=True,
        )
        copy = os.path.join(os.path.realpath(tmp), "inventories")
        shutil.copytree(os.path.join(GOLDEN, "inventories"), copy)
        os.mkdir(os.path.join(copy, "empty"))
        for name, case in cases.items():
            args = [word for source in case["sources"] for word in ("-i", source)]
            # What the command is also given, like a vault password file.
            args += case.get("args", [])
            env = case.get("env", {})
            # `$COPY` is this copy of the directory and `$PATH` the search path the run has.
            used = {k: v.replace("$COPY", copy).replace("$PATH", os.environ["PATH"]) for k, v in env.items()}
            listed = run(["ansible-inventory", *args, "--list"], cwd=copy, extra_env=used)
            # A group walk over sets of groups hashed by name: the host order is not stable.
            unordered = case.get("hosts") == "unordered"
            hosts = None if unordered else run(
                ["ansible", *args, "all", "--list-hosts"], cwd=copy, extra_env=used
            )
            dump = json.loads(listed.stdout) if listed.returncode == 0 else None
            # The command's own bytes: --list for every case, and --host for the first host of
            # the dump. A walk of hashed groups gives no stable order to compare bytes with.
            host = None
            if dump and dump["_meta"]["hostvars"] and not unordered:
                first = sorted(dump["_meta"]["hostvars"])[0]
                one = run(["ansible-inventory", *args, "--host", first], cwd=copy, extra_env=used)
                host = {"name": first, "rc": one.returncode, "stdout": one.stdout.replace(copy, TMP)}
            def shown(result):
                return {
                    "rc": result.returncode,
                    "stdout": result.stdout.replace(copy, TMP),
                    "stderr": result.stderr.replace(copy, TMP),
                }

            graphs = None
            if not unordered and not case.get("no_graph"):
                graphs = {
                    "graph": shown(run(["ansible-inventory", *args, "--graph"], cwd=copy, extra_env=used)),
                    "graph_vars": shown(
                        run(["ansible-inventory", *args, "--graph", "--vars"], cwd=copy, extra_env=used)
                    ),
                    "graph_vars_export": shown(
                        run(["ansible-inventory", *args, "--graph", "--vars", "--export"], cwd=copy, extra_env=used)
                    ),
                }
                # The cases that name a group, and the one without the options they were given.
                for group in case.get("graph_groups", []):
                    graphs[f"graph_{group}"] = shown(
                        run(["ansible-inventory", *args, "--graph", group, "--vars"], cwd=copy, extra_env=used)
                    )
                if case.get("args"):
                    bare = [word for source in case["sources"] for word in ("-i", source)]
                    graphs["graph_vars_bare"] = shown(
                        run(["ansible-inventory", *bare, "--graph", "--vars"], cwd=copy, extra_env=used)
                    )
            # `--yaml`, `--export` and both, for `--list` and for the first host.
            dumps = None

            def brief(result):
                # The stderr of a dump is not compared; a crash's traceback names the machine.
                recorded = shown(result)
                recorded["stderr"] = recorded["stderr"].split("\n\n")[0]
                return recorded

            if not unordered:
                options = {"yaml": ["--yaml"], "toml": ["--toml"]}
                if not case.get("no_export"):
                    options.update(
                        {
                            "export": ["--export"],
                            "yaml_export": ["--yaml", "--export"],
                            "toml_export": ["--toml", "--export"],
                        }
                    )
                with_lib = {**used, "PYTHONPATH": lib}
                dumps = {
                    key: brief(
                        run(
                            ["ansible-inventory", *args, "--list", *extra],
                            cwd=copy,
                            extra_env=with_lib if "toml" in key else used,
                        )
                    )
                    for key, extra in options.items()
                }
                if host:
                    for key, extra in options.items():
                        dumps[f"host_{key}"] = brief(
                            run(
                                ["ansible-inventory", *args, "--host", host["name"], *extra],
                                cwd=copy,
                                extra_env=with_lib if "toml" in key else used,
                            )
                        )
            nosuch = None
            if case.get("cli"):
                missing = run(["ansible-inventory", *args, "--host", "nosuch"], cwd=copy, extra_env=used)
                nosuch = {
                    "rc": missing.returncode,
                    "stdout": missing.stdout,
                    "stderr": missing.stderr.replace(copy, TMP),
                }
            recorded[name] = {
                "cli_only": bool(case.get("cli")),
                "list_stdout": None if unordered else listed.stdout.replace(copy, TMP),
                "host": host,
                "graphs": graphs,
                "dumps": dumps,
                "args": case.get("args", []),
                "nosuch": nosuch,
                "sources": case["sources"],
                "env": env,
                "needs": case.get("needs", []),
                "rc": listed.returncode,
                "stderr": listed.stderr.replace(copy, TMP),
                "list": dump,
                "hosts": None
                if unordered
                else [h.strip() for h in hosts.stdout.splitlines()[1:] if h.strip()],
            }
    return recorded


def record():
    inventory = os.path.join(GOLDEN, "inventory.ini")
    listed = run(["ansible-inventory", "-i", inventory, "--list"])
    if listed.returncode:
        raise Failed(f"ansible-inventory exited {listed.returncode} on inventory.ini\n{listed.stderr}")
    dump = json.loads(listed.stdout)
    hostvars = dump.get("_meta", {}).get("hostvars", {})
    patterns = {}
    with open(os.path.join(GOLDEN, "patterns.txt"), encoding="utf-8") as f:
        for line in f:
            pattern = line.strip()
            if pattern:
                patterns[pattern] = _list_hosts(inventory, pattern)
    groups = {name: sorted(body.get("hosts", [])) for name, body in dump.items() if name != "_meta" and "hosts" in body}
    # Its own minimal fixture, where a group and one of its hosts share a name: the one case that
    # must warn, kept apart so that inventory.ini's patterns can show that nothing there warns.
    homonym = _list_hosts(os.path.join(GOLDEN, "homonym_inventory.ini"), "same")
    write_json(
        os.path.join(GOLDEN, "expected_inventory.json"),
        {
            "hostvars": hostvars,
            "groups": groups,
            "patterns": patterns,
            "homonym": homonym,
            "sources": _sources(),
        },
    )
    print(f"inventory golden: {len(hostvars)} hosts, {len(patterns)} patterns")
