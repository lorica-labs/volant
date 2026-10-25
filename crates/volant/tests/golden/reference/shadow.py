# SPDX-License-Identifier: GPL-3.0-or-later
"""The `shadow` kind: chosen scenarios run by `ansible-playbook` with the `volant.compat.record`
callback, each recording stored gzipped as `shadow/<scenario>.jsonl.gz` for `shadow_golden.rs`
to replay. A scenario runs in a copy at a fixed path, so the paths it records are the same on
every machine; the times a module reports, an async job's ids and process, the tasks' uuids
and the recording account's home are normalised. Then the
callback's own sample is recorded under every ansible-core it supports and compared
(`just compat-matrix`).
"""
import gzip
import json
import os
import re
import shutil
import subprocess

from . import GOLDEN, SCENARIOS
from .run import EMPTY_CONFIG, Failed, run
from .scenario import load

SHADOW = os.path.join(GOLDEN, "shadow")
COLLECTIONS = os.path.normpath(os.path.join(GOLDEN, "..", "..", "..", "..", "collections"))
# Where each scenario runs: the recording names its files under it.
ROOT = "/tmp/volant-golden-shadow"

# Roles, handlers, loops, `until`, `delegate_to`, `serial`, `run_once`, includes and imports,
# `no_log` and a failure a `rescue` takes. The async scenarios wait for #920.
SCENARIOS_RECORDED = [
    "blocks-failures",
    "blocks-nested",
    "delegate-run-once",
    "delegate-run-once-serial",
    "handlers-chained",
    "handlers-definition-order",
    "handlers-flush-in-include",
    "handlers-flush-in-rescue",
    "handlers-play-no-log",
    "include-apply",
    "include-role-public",
    "include-splices",
    "loop-control-extended",
    "loops-results",
    "loops-skipped",
    "retries-results",
    "until-attempts",
]

# What a module reports that differs on every run, what an async job is named by, and the
# interpreter the host was found to have.
VOLATILE = {
    "start": "<start>",
    "end": "<end>",
    "delta": "<delta>",
    "started": "<started>",
    "discovered_interpreter_python": "<python>",
}


# The id ansible-core gives an async job: `j`, random digits, a dot and its process id. A
# playbook's own made-up ids (`nosuch.1`) do not have that shape.
JOB_ID = re.compile(r"j\d{6,}\.\d+")


def _normalised(value):
    if isinstance(value, dict):
        return {
            k: VOLATILE[k] if k in VOLATILE and isinstance(v, str) else "<pid>" if k == "child_pid" else _normalised(v)
            for k, v in value.items()
        }
    if isinstance(value, list):
        return [_normalised(v) for v in value]
    return value


def record_one(name):
    directory = os.path.join(SCENARIOS, name)
    case = load(directory)
    if case["program"] != "ansible-playbook" or case["needs"] or case["links"]:
        raise Failed(f"shadow {name}: only a plain ansible-playbook scenario can be recorded")
    copy = os.path.join(ROOT, name)
    shutil.rmtree(copy, ignore_errors=True)
    shutil.copytree(directory, copy, symlinks=True, ignore=shutil.ignore_patterns("case.yml", "expected.json"))
    own = os.path.join(copy, "ansible.cfg")
    config = own if os.path.exists(own) else EMPTY_CONFIG
    forks = [] if case["forks"] is None else ["--forks", str(case["forks"])]
    recording = os.path.join(copy, "recording.jsonl")
    env = dict(case["env"])
    env.update(
        ANSIBLE_COLLECTIONS_PATH=COLLECTIONS,
        ANSIBLE_CALLBACKS_ENABLED="volant.compat.record",
        VOLANT_RECORD_PATH=recording,
    )
    run(["ansible-playbook", *case["runs"][0]["args"], *forks], cwd=copy, config=config, extra_env=env)
    if not os.path.exists(recording):
        raise Failed(f"shadow {name}: the run wrote no recording")
    with open(recording, encoding="utf-8") as f:
        text = f.read()
    # An async job's id is drawn anew by every run and travels into the arguments and the status
    # file paths of the tasks that wait for it: each becomes its rank of first appearance,
    # wherever it is written.
    for n, job in enumerate(dict.fromkeys(JOB_ID.findall(text)), start=1):
        text = text.replace(job, f"<job-{n}>")
    # The interpreter the hosts were found to have lives under the recording account's home,
    # which no recording names.
    text = text.replace(os.path.expanduser("~"), "<home>")
    events = [_normalised(json.loads(line)) for line in text.splitlines() if line.strip()]
    # A task's uuid is drawn anew by every run: each becomes its rank of first appearance.
    uuids = {}
    for event in events:
        for key in ("uuid", "task"):
            if isinstance(event.get(key), str):
                event[key] = uuids.setdefault(event[key], f"task-{len(uuids) + 1}")
    os.makedirs(SHADOW, exist_ok=True)
    target = os.path.join(SHADOW, f"{name}.jsonl.gz")
    text = "".join(json.dumps(e, sort_keys=False) + "\n" for e in events).encode()
    # No name and no time in the gzip header: a second run writes the same bytes.
    with open(target, "wb") as raw, gzip.GzipFile(filename="", mode="wb", fileobj=raw, mtime=0) as out:
        out.write(text)
    shutil.rmtree(copy, ignore_errors=True)
    return len(text), os.path.getsize(target)


def record():
    total_plain = total_gzip = 0
    for name in SCENARIOS_RECORDED:
        plain, packed = record_one(name)
        total_plain += plain
        total_gzip += packed
    print(f"shadow goldens: {len(SCENARIOS_RECORDED)} recordings, {total_plain} bytes, {total_gzip} gzipped")
    matrix = subprocess.run(["just", "compat-matrix"], cwd=os.path.join(GOLDEN, "..", "..", "..", ".."))
    if matrix.returncode:
        raise Failed("the callback records other kinds of event or keys under another ansible-core")
