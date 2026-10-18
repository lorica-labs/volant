# SPDX-License-Identifier: GPL-3.0-or-later
"""The `inventory` kind: inventory.ini read by `ansible-inventory`, every line of patterns.txt
resolved by `ansible --list-hosts`, and the homonym fixture, recorded in expected_inventory.json."""
import json
import os
import shutil
import tempfile

import yaml

from . import GOLDEN
from .redact import TMP
from .run import Failed, run, write_json

HOMONYM_WARNING = "Found both group and host with same name"


def _list_hosts(inventory, pattern):
    result = run(["ansible", "-i", inventory, pattern, "--list-hosts"])
    hosts = [h.strip() for h in result.stdout.splitlines()[1:] if h.strip()]
    # The homonym warning alone, not any warning: an unmatched term or an empty result warn too,
    # which the engine keeps in `unmatched`, not `warnings`.
    return {"hosts": hosts, "warning": HOMONYM_WARNING in result.stderr}


def _sources():
    """Every case of inventories/cases.yml, read in a copy of the directory (plus the empty
    directory git cannot hold): rc, stderr, the --list dump and the order --list-hosts gives."""
    with open(os.path.join(GOLDEN, "inventories", "cases.yml"), encoding="utf-8") as f:
        cases = yaml.safe_load(f)
    recorded = {}
    with tempfile.TemporaryDirectory() as tmp:
        copy = os.path.join(os.path.realpath(tmp), "inventories")
        shutil.copytree(os.path.join(GOLDEN, "inventories"), copy)
        os.mkdir(os.path.join(copy, "empty"))
        for name, case in cases.items():
            args = [word for source in case["sources"] for word in ("-i", source)]
            env = case.get("env", {})
            # `$COPY` is this copy of the directory and `$PATH` the search path the run has.
            used = {k: v.replace("$COPY", copy).replace("$PATH", os.environ["PATH"]) for k, v in env.items()}
            listed = run(["ansible-inventory", *args, "--list"], cwd=copy, extra_env=used)
            # A group walk over sets of groups hashed by name: the host order is not stable.
            unordered = case.get("hosts") == "unordered"
            hosts = None if unordered else run(
                ["ansible", *args, "all", "--list-hosts"], cwd=copy, extra_env=used
            )
            recorded[name] = {
                "sources": case["sources"],
                "env": env,
                "needs": case.get("needs", []),
                "rc": listed.returncode,
                "stderr": listed.stderr.replace(copy, TMP),
                "list": json.loads(listed.stdout) if listed.returncode == 0 else None,
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
