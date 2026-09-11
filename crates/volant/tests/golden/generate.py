# SPDX-License-Identifier: GPL-3.0-or-later
"""Runs every case in cases.yml through the reference ansible-playbook and records what it did.

The interpreter comes from the ansible-core tool environment, so PyYAML is available.
"""
import json
import os
import shlex
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
            # sort_keys=False for the same reason as the json.dump below: safe_dump sorts every
            # mapping by default, which would hand the reference a case's `vars` in an order
            # cases.yml never wrote and silently make key order untestable.
            yaml.safe_dump(play, f, default_flow_style=False, allow_unicode=True, sort_keys=False)
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
    # Deliberately not sort_keys: a case's `vars` must reach the Rust side in the order
    # cases.yml wrote them, or no case could measure what a filter does to key order.
    with open(os.path.join(HERE, "expected.json"), "w", encoding="utf-8") as f:
        json.dump(results, f, indent=2, ensure_ascii=False)
        f.write("\n")
    print(f"{len(results)} cases recorded against ansible-core {REFERENCE}")
    return inventory() or listings()


HOMONYM_WARNING = "Found both group and host with same name"


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
            # Specifically the homonym warning, not any "WARNING" substring: ansible also warns
            # on stderr for an unmatched term or an empty overall result ("Could not match
            # supplied host pattern", "No hosts matched"), which is a distinct concept our
            # Resolution keeps in `unmatched`, not `warnings`. Conflating the two here would make
            # this field warn for reasons resolve() was never asked to reproduce.
            patterns[pattern] = {"hosts": hosts, "warning": HOMONYM_WARNING in run.stderr}
    groups = {name: sorted(body.get("hosts", [])) for name, body in dump.items() if name != "_meta" and "hosts" in body}
    homonym = homonym_warning()
    with open(os.path.join(HERE, "expected_inventory.json"), "w", encoding="utf-8") as f:
        json.dump(
            {"hostvars": hostvars, "groups": groups, "patterns": patterns, "homonym": homonym},
            f,
            indent=2,
            ensure_ascii=False,
            sort_keys=True,
        )
        f.write("\n")
    print(f"inventory golden: {len(hostvars)} hosts, {len(patterns)} patterns")
    return 0


def listings():
    """What `--list-tasks`, `--list-tags`, `--list-hosts` and `--syntax-check` print, byte for
    byte, for every invocation in listing/args.txt.

    The fixtures in listing/ are written so this recording can be compared with anything: no
    play carries more than one tag and no play used with --list-hosts matches more than one
    host, because the reference prints both out of a Python set and a set of two short strings
    iterates in an order that changes from one process to the next (measured: four distinct
    orders in eight runs of the same command). Adding a second tag or a second host to one of
    those plays would make this golden flap rather than fail.

    Only stdout and the exit code are recorded. stderr carries warnings whose wording is a
    separate question from the layout this gate exists to pin.
    """
    here = os.path.join(HERE, "listing")
    env = dict(os.environ, ANSIBLE_NOCOLOR="1")
    # An ansible.cfg anywhere above this directory would otherwise change the answers - roles
    # path, tags, anything. The recording has to depend on the fixtures alone.
    env.pop("ANSIBLE_CONFIG", None)
    for name in list(env):
        if name.startswith("ANSIBLE_"):
            del env[name]
    env["ANSIBLE_NOCOLOR"] = "1"
    expected = {}
    with open(os.path.join(here, "args.txt"), encoding="utf-8") as f:
        for line in f:
            line = line.strip()
            if not line or line.startswith("#"):
                continue
            run = subprocess.run(
                ["ansible-playbook", "-i", "inv.ini", *shlex.split(line)],
                cwd=here, env=env, capture_output=True, text=True,
            )
            expected[line] = {"stdout": run.stdout, "code": run.returncode}
    with open(os.path.join(HERE, "expected_listing.json"), "w", encoding="utf-8") as f:
        json.dump(expected, f, indent=2, ensure_ascii=False, sort_keys=True)
        f.write("\n")
    print(f"listing golden: {len(expected)} invocations recorded")
    return 0


def homonym_warning():
    """A dedicated, minimal fixture where a group and one of its own hosts share a name: the one
    case in this golden that must warn, kept apart from inventory.ini so that fixture's patterns
    can test the opposite (that nothing there warns) without every one of them being drowned out
    by a single homonym anywhere in the inventory triggering Ansible's load-time warning."""
    inv = os.path.join(HERE, "homonym_inventory.ini")
    run = subprocess.run(["ansible", "-i", inv, "same", "--list-hosts"], capture_output=True, text=True)
    hosts = [h.strip() for h in run.stdout.splitlines()[1:] if h.strip()]
    return {"hosts": hosts, "warning": HOMONYM_WARNING in run.stderr}


if __name__ == "__main__":
    sys.exit(main())
