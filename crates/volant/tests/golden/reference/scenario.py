# SPDX-License-Identifier: GPL-3.0-or-later
"""The `scenario` kind: every `tests/scenarios/<name>/case.yml`, run by `ansible-playbook` in a
copy of its directory, its exit code and stdout recorded in `expected.json`.

case.yml holds `args` (the argv after `ansible-playbook`), or `runs`, a list of `{args: [...]}`
whose `expected.json` is then a list in the same order; `env`, extra environment for both engines;
`forks`, 1 unless said, so that hosts print in the inventory's order under both engines; `needs`,
any of `ansible-core`, `sudo`, `systemd`; `unordered`, regexes of the lines whose order may differ
once `forks` is above 1. The copy's path reads `<golden-tmp>` in stdout, and nothing else is
normalised.
"""
import os
import re
import shutil
import subprocess

import yaml

from . import SCENARIOS
from .redact import TMP
from .run import EMPTY_CONFIG, Failed, run, scratch, write_json

KEYS = {"args", "runs", "env", "forks", "needs", "unordered"}
NEEDS = {"ansible-core", "sudo", "systemd"}

# The two things the reference prints out of a Python set, in an order that changes between
# processes: a play line's tags (`','.join(set(play.tags))`) and the host list of `--list-hosts`.
PLAY_TAGS = re.compile(r"^  play #\d+ .*\tTAGS: \[(.*)\]$")
HOST_COUNT = re.compile(r"^    hosts \((\d+)\):$")

# The block `-vv` opens with, naming the machine's paths and interpreter. The test replaces
# Volant's own version line with the same placeholder.
VERSION_BLOCK = re.compile(r"^ansible-playbook \[core [^\]\n]*\]\n(?:  .*\n){8}", re.M)


def unreproducible(stdout):
    """What in this output the reference would print in another order next time, or None."""
    for line in stdout.splitlines():
        tags = PLAY_TAGS.match(line)
        if tags and "," in tags.group(1):
            return f"a play line carries more than one tag: {line.strip()}"
        hosts = HOST_COUNT.match(line)
        if hosts and int(hosts.group(1)) >= 2:
            return f"a play matches more than one host: {line.strip()}"
    return None


def _strings(value):
    return isinstance(value, list) and all(isinstance(v, str) for v in value)


def load(directory):
    """The scenario's case.yml, checked: a key or a value this generator cannot honour is refused."""
    name = os.path.basename(directory)
    path = os.path.join(directory, "case.yml")
    if not os.path.exists(path):
        raise Failed(f"scenario {name}: no case.yml")
    with open(path, encoding="utf-8") as f:
        case = yaml.safe_load(f)
    if not isinstance(case, dict):
        raise Failed(f"scenario {name}: case.yml is not a mapping")
    unknown = sorted(set(case) - KEYS)
    if unknown:
        raise Failed(f"scenario {name}: {', '.join(unknown)} is not supported in case.yml")
    if ("args" in case) == ("runs" in case):
        raise Failed(f"scenario {name}: case.yml needs `args` or `runs`, not both or neither")
    if "args" in case:
        runs = [{"args": case["args"]}]
    else:
        runs = case["runs"]
        if not isinstance(runs, list) or not runs:
            raise Failed(f"scenario {name}: `runs` is not a list of runs")
    for one in runs:
        if not isinstance(one, dict) or set(one) != {"args"} or not _strings(one["args"]):
            raise Failed(f"scenario {name}: a run is not `{{args: [...]}}` with string arguments: {one!r}")
    env = case.get("env") or {}
    if not isinstance(env, dict) or not all(isinstance(k, str) and isinstance(v, str) for k, v in env.items()):
        raise Failed(f"scenario {name}: `env` is not a mapping of strings")
    forks = case.get("forks", 1)
    if isinstance(forks, bool) or not isinstance(forks, int) or forks < 1:
        raise Failed(f"scenario {name}: `forks` is not a positive integer")
    needs = case.get("needs") or []
    if not _strings(needs) or set(needs) - NEEDS:
        raise Failed(f"scenario {name}: `needs` takes {', '.join(sorted(NEEDS))}, not {needs!r}")
    unordered = case.get("unordered") or []
    if not _strings(unordered):
        raise Failed(f"scenario {name}: `unordered` is not a list of regexes")
    if unordered and forks == 1:
        raise Failed(f"scenario {name}: `unordered` excuses nothing under `forks: 1`")
    for pattern in unordered:
        try:
            re.compile(pattern)
        except re.error as e:
            raise Failed(f"scenario {name}: `unordered` {pattern!r} is not a regex ({e})") from None
    return {"runs": runs, "env": env, "forks": forks, "needs": needs, "single": "args" in case}


def _missing(needs):
    """The needs this machine cannot meet; the generator itself runs the recorded ansible-core."""
    missing = []
    if "sudo" in needs and subprocess.run(["sudo", "-n", "true"], capture_output=True).returncode:
        missing.append("sudo")
    if "systemd" in needs and not os.path.isdir("/run/systemd/system"):
        missing.append("systemd")
    return missing


def record_one(directory):
    name = os.path.basename(directory)
    case = load(directory)
    missing = _missing(case["needs"])
    if missing:
        raise Failed(f"scenario {name}: this machine lacks {', '.join(missing)}")
    results = []
    for one in case["runs"]:
        # A fresh copy per run: a run may change the files the next one reads.
        with scratch() as tmp:
            copy = os.path.join(tmp, name)
            shutil.copytree(directory, copy, symlinks=True, ignore=shutil.ignore_patterns("case.yml", "expected.json"))
            own = os.path.join(copy, "ansible.cfg")
            config = own if os.path.exists(own) else EMPTY_CONFIG
            argv = ["ansible-playbook", *one["args"], "--forks", str(case["forks"])]
            result = run(argv, cwd=copy, config=config, extra_env=case["env"])
            stdout = VERSION_BLOCK.sub("<golden-version>\n", result.stdout.replace(copy, TMP))
            # The recording's own callback, which the reference sets aside like the two it ships.
            stdout = stdout.replace("Skipping callback 'golden_json', as we already have a stdout callback.\n", "")
        bad = unreproducible(stdout)
        if bad:
            raise Failed(
                f"scenario {name}, {' '.join(one['args'])}: {bad}. The reference prints a play's "
                "tags and hosts out of a Python set: reshape the fixture so the play carries one "
                "tag and matches one host, or list it under a --limit that leaves one."
            )
        results.append({"code": result.returncode, "stdout": stdout})
    write_json(os.path.join(directory, "expected.json"), results[0] if case["single"] else results)
    return len(results)


def record():
    directories = sorted(
        os.path.join(SCENARIOS, entry)
        for entry in os.listdir(SCENARIOS)
        if os.path.isdir(os.path.join(SCENARIOS, entry))
    )
    runs = sum(record_one(directory) for directory in directories)
    print(f"scenario goldens: {len(directories)} scenarios, {runs} runs recorded")
