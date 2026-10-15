# SPDX-License-Identifier: GPL-3.0-or-later
"""The `inventory` kind: inventory.ini read by `ansible-inventory`, every line of patterns.txt
resolved by `ansible --list-hosts`, and the homonym fixture, recorded in expected_inventory.json."""
import json
import os

from . import GOLDEN
from .run import Failed, run, write_json

HOMONYM_WARNING = "Found both group and host with same name"


def _list_hosts(inventory, pattern):
    result = run(["ansible", "-i", inventory, pattern, "--list-hosts"])
    hosts = [h.strip() for h in result.stdout.splitlines()[1:] if h.strip()]
    # The homonym warning alone, not any warning: an unmatched term or an empty result warn too,
    # which the engine keeps in `unmatched`, not `warnings`.
    return {"hosts": hosts, "warning": HOMONYM_WARNING in result.stderr}


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
        {"hostvars": hostvars, "groups": groups, "patterns": patterns, "homonym": homonym},
    )
    print(f"inventory golden: {len(hostvars)} hosts, {len(patterns)} patterns")
