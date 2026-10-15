# SPDX-License-Identifier: GPL-3.0-or-later
"""The `template` kind: every row of cases.yml as one `debug` task, recorded in expected.json."""
import os

import yaml

from . import GOLDEN, REFERENCE
from .run import json_report, outcomes, run, scratch, write_json


def record():
    with open(os.path.join(GOLDEN, "cases.yml"), encoding="utf-8") as f:
        cases = yaml.safe_load(f)
    tasks = []
    payloads = {}
    for i, case in enumerate(cases):
        # A case naming `untrusted` needs those variables to have come from the host, which is what
        # the reference decides trust on: each is read out of a file by `command`, since a literal
        # in the task's own text would be rendered before the command ran.
        #
        # The register carries the case's number, and the case's own `vars:` alias it under the
        # name cases.yml wrote: a register outlives its task, and a name written plain would stay
        # defined for every later case.
        case_vars = dict(case.get("vars") or {})
        for name, text in case.get("untrusted", {}).items():
            payloads[f"{i}-{name}"] = text
            register = f"raw_{i}_{name}"
            tasks.append({
                "name": f"setup {i} {name}",
                "command": "cat {{ payload_dir }}/" + f"{i}-{name}.txt",
                "register": register,
            })
            case_vars[name] = f"{{{{ {register}.stdout }}}}"
        task = {"name": f"case {i}", "ignore_errors": True}
        if case_vars:
            task["vars"] = case_vars
        if "when" in case:
            task["debug"] = {"msg": "ran"}
            task["when"] = case["when"]
        else:
            task["debug"] = {"msg": case["template"]}
        tasks.append(task)
    play = [{"hosts": "localhost", "gather_facts": False, "connection": "local", "tasks": tasks}]
    with scratch() as tmp:
        play[0]["vars"] = {"payload_dir": tmp}
        for stem, text in payloads.items():
            with open(os.path.join(tmp, f"{stem}.txt"), "w", encoding="utf-8") as f:
                f.write(text)
        playbook = os.path.join(tmp, "golden.yml")
        with open(playbook, "w", encoding="utf-8") as f:
            # Not sorted: a case's `vars` must reach the reference in the order cases.yml wrote.
            yaml.safe_dump(play, f, default_flow_style=False, allow_unicode=True, sort_keys=False)
        with open(os.path.join(tmp, "golden_lookup.txt"), "w", encoding="utf-8") as f:
            f.write("file contents")
        result = run(
            ["ansible-playbook", "-i", "localhost,", playbook],
            extra_env={"ANSIBLE_STDOUT_CALLBACK": "golden_json", "VOLANT_GOLDEN_ENV": "golden-env-value"},
        )
    # By name: a case with setup tasks in front of it would read another case's answer by position.
    found = outcomes(json_report(result, "the template cases"))
    results = []
    for i, case in enumerate(cases):
        outcome = found[f"case {i}"]
        entry = {"case": case}
        if outcome.get("skipped"):
            entry["skipped"] = True
        elif outcome.get("failed"):
            # A few of the reference's messages quote the playbook's random directory.
            error = outcome.get("msg", "")
            entry["error"] = error.replace(tmp, "<tmpdir>") if isinstance(error, str) else error
        else:
            entry["result"] = outcome.get("msg")
        results.append(entry)
    # Not sorted either: a filter's effect on key order is measured through these `vars`.
    write_json(os.path.join(GOLDEN, "expected.json"), results, sort_keys=False)
    print(f"{len(results)} cases recorded against ansible-core {REFERENCE}")
