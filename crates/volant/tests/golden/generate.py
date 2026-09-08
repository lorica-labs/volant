# SPDX-License-Identifier: GPL-3.0-or-later
"""Runs every case in cases.yml through the reference ansible-playbook and records what it did.

The interpreter comes from the ansible-core tool environment, so PyYAML is available.
"""
import json
import os
import subprocess
import sys
import tempfile

import yaml

HERE = os.path.dirname(os.path.abspath(__file__))
REFERENCE = open(os.path.join(HERE, "ANSIBLE_VERSION"), encoding="utf-8").read().strip()


def main() -> int:
    version = subprocess.run(["ansible-playbook", "--version"], capture_output=True, text=True, check=True).stdout
    if REFERENCE not in version.splitlines()[0]:
        print(f"ansible-playbook is not {REFERENCE}: {version.splitlines()[0]}", file=sys.stderr)
        return 1
    with open(os.path.join(HERE, "cases.yml"), encoding="utf-8") as f:
        cases = yaml.safe_load(f)
    tasks = []
    for i, case in enumerate(cases):
        task = {"name": f"case {i}", "ignore_errors": True}
        if "vars" in case:
            task["vars"] = case["vars"]
        if "when" in case:
            task["debug"] = {"msg": "ran"}
            task["when"] = case["when"]
        else:
            task["debug"] = {"msg": case["template"]}
        tasks.append(task)
    play = [{"hosts": "localhost", "gather_facts": False, "connection": "local", "tasks": tasks}]
    env = dict(os.environ, ANSIBLE_STDOUT_CALLBACK="ansible.builtin.json", ANSIBLE_NOCOLOR="1")
    env["VOLANT_GOLDEN_ENV"] = "golden-env-value"
    with tempfile.TemporaryDirectory() as tmp:
        playbook = os.path.join(tmp, "golden.yml")
        with open(playbook, "w", encoding="utf-8") as f:
            yaml.safe_dump(play, f, default_flow_style=False, allow_unicode=True)
        with open(os.path.join(tmp, "golden_lookup.txt"), "w", encoding="utf-8") as f:
            f.write("file contents")
        run = subprocess.run(["ansible-playbook", "-i", "localhost,", playbook], env=env, capture_output=True, text=True)
    report = json.loads(run.stdout)
    results = []
    for case, task in zip(cases, report["plays"][0]["tasks"]):
        outcome = task["hosts"]["localhost"]
        entry = {"case": case}
        if outcome.get("skipped"):
            entry["skipped"] = True
        elif outcome.get("failed"):
            entry["error"] = outcome.get("msg", "")
        else:
            entry["result"] = outcome.get("msg")
        results.append(entry)
    with open(os.path.join(HERE, "expected.json"), "w", encoding="utf-8") as f:
        json.dump(results, f, indent=2, ensure_ascii=False, sort_keys=True)
        f.write("\n")
    print(f"{len(results)} cases recorded against ansible-core {REFERENCE}")
    return inventory()


def inventory():
    inv = os.path.join(HERE, "inventory.ini")
    dump = json.loads(subprocess.run(["ansible-inventory", "-i", inv, "--list"], capture_output=True, text=True, check=True).stdout)
    hostvars = dump.get("_meta", {}).get("hostvars", {})
    patterns = {}
    with open(os.path.join(HERE, "patterns.txt"), encoding="utf-8") as f:
        for line in f:
            pattern = line.strip()
            if not pattern:
                continue
            run = subprocess.run(["ansible", "-i", inv, pattern, "--list-hosts"], capture_output=True, text=True)
            hosts = [h.strip() for h in run.stdout.splitlines()[1:] if h.strip()]
            patterns[pattern] = {"hosts": hosts, "warning": "WARNING" in run.stderr}
    groups = {name: sorted(body.get("hosts", [])) for name, body in dump.items() if name != "_meta" and "hosts" in body}
    with open(os.path.join(HERE, "expected_inventory.json"), "w", encoding="utf-8") as f:
        json.dump({"hostvars": hostvars, "groups": groups, "patterns": patterns}, f, indent=2, ensure_ascii=False, sort_keys=True)
        f.write("\n")
    print(f"inventory golden: {len(hostvars)} hosts, {len(patterns)} patterns")
    return 0


if __name__ == "__main__":
    sys.exit(main())
