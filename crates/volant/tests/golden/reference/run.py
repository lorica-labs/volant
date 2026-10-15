# SPDX-License-Identifier: GPL-3.0-or-later
"""The one way the generator runs the reference: an environment of its own, a deadline, a scratch
directory, the native lock and the collection pins."""
import contextlib
import fcntl
import json
import os
import signal
import subprocess
import tempfile

from . import GOLDEN, REFERENCE

# Read by every run instead of the account's own `~/.ansible.cfg`, unless a scenario brings one.
EMPTY_CONFIG = os.path.join(os.path.dirname(os.path.abspath(__file__)), "ansible.cfg")
CALLBACKS = os.path.join(GOLDEN, "callback_plugins")
# Kept from the caller, as reference_modules.rs keeps them: the `ssh` job installs the pinned collections
# outside every default path and names them here.
FORWARDED = ("ANSIBLE_COLLECTIONS_PATH", "ANSIBLE_COLLECTIONS_PATHS")
# The native play is the longest run: under a minute on the development machine.
DEADLINE = 300
# Held from the machine checks to the last read-back of the module play: its directories, accounts,
# unit and package are the whole machine's, and reference_modules.rs takes the same lock. Under the account's
# own home, where no other account can create it first.
NATIVE_LOCK = os.path.join(os.path.expanduser("~"), ".cache", "volant-golden-native.lock")


class Failed(Exception):
    """A run that cannot be recorded, with the reason."""


def environment(config=EMPTY_CONFIG, extra=None):
    """This process's environment without any `ANSIBLE_*` but the collection paths, then the
    reference's own settings and `extra`."""
    env = {k: v for k, v in os.environ.items() if not k.startswith("ANSIBLE_") or k in FORWARDED}
    env.update(ANSIBLE_CONFIG=config, ANSIBLE_NOCOLOR="1", ANSIBLE_CALLBACK_PLUGINS=CALLBACKS)
    env.update(extra or {})
    return env


def run(argv, cwd=None, config=EMPTY_CONFIG, extra_env=None, deadline=DEADLINE):
    """`argv` under `environment()`, its stdout and stderr captured. The whole process group is
    killed at the deadline: a module's children hold the same pipes."""
    process = subprocess.Popen(
        argv,
        cwd=cwd,
        env=environment(config, extra_env),
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
        start_new_session=True,
    )
    try:
        stdout, stderr = process.communicate(timeout=deadline)
    except subprocess.TimeoutExpired:
        os.killpg(process.pid, signal.SIGKILL)
        process.communicate()
        raise Failed(f"{' '.join(argv)} did not finish within {deadline} s") from None
    return subprocess.CompletedProcess(argv, process.returncode, stdout, stderr)


def json_report(result, what):
    """The JSON report a `golden_json` run printed, or `Failed` naming what did not run."""
    if result.returncode:
        raise Failed(f"ansible-playbook exited {result.returncode} while recording {what}\n{result.stdout[-4000:]}\n{result.stderr}")
    try:
        return json.loads(result.stdout)
    except json.JSONDecodeError as e:
        raise Failed(f"the report of {what} is not JSON ({e})\n{result.stderr}") from None


def outcomes(report):
    """The first play's results keyed by task name, for `localhost`."""
    return {task["task"]["name"]: task["hosts"]["localhost"] for task in report["plays"][0]["tasks"]}


@contextlib.contextmanager
def scratch():
    """A directory removed once the block is done."""
    with tempfile.TemporaryDirectory(prefix="volant-golden-") as directory:
        yield directory


@contextlib.contextmanager
def native_lock():
    os.makedirs(os.path.dirname(NATIVE_LOCK), exist_ok=True)
    with open(NATIVE_LOCK, "a") as lock:
        fcntl.flock(lock, fcntl.LOCK_EX)
        yield


def write_json(path, value, sort_keys=True, indent=2):
    os.makedirs(os.path.dirname(path), exist_ok=True)
    with open(path, "w", encoding="utf-8") as f:
        json.dump(value, f, indent=indent, ensure_ascii=False, sort_keys=sort_keys)
        f.write("\n")


def _read_pins(name):
    pins = {}
    with open(os.path.join(GOLDEN, name), encoding="utf-8") as f:
        for line in f:
            line = line.strip()
            if line:
                key, version = line.split()
                pins[key] = version
    return pins


# The collection versions the goldens are recorded against, plus `netaddr`: not a collection, but
# pinned the same way because `ansible.utils.ipwrap` needs it in the same controller environment.
COLLECTIONS = _read_pins("COLLECTIONS")


def _installed_collections():
    """What the controller's ansible-core has, read the way it reads it: a collection's version from
    its own `MANIFEST.json`, on the first configured collection path that carries it; `netaddr` is a
    plain import next to it. Read in this process, whose environment `check_reference` made the
    runs' own."""
    from ansible import constants as ansible_constants

    versions = {}
    for fqcn in COLLECTIONS:
        if fqcn == "netaddr":
            continue
        namespace, name = fqcn.split(".", 1)
        for root in ansible_constants.COLLECTIONS_PATHS:
            manifest = os.path.join(os.path.expanduser(root), "ansible_collections", namespace, name, "MANIFEST.json")
            if os.path.exists(manifest):
                with open(manifest, encoding="utf-8") as f:
                    versions[fqcn] = json.load(f)["collection_info"]["version"]
                break
    try:
        import netaddr

        versions["netaddr"] = netaddr.__version__
    except ImportError:
        pass
    return versions


def collection_mismatches():
    """What in COLLECTIONS the controller does not have installed, one line per pin."""
    installed = _installed_collections()
    return [
        f"{name} is {installed.get(name, 'not installed')}, not {version}"
        for name, version in COLLECTIONS.items()
        if installed.get(name) != version
    ]


def check_reference():
    """Gives this process the runs' environment, then checks the reference and its collections."""
    env = environment()
    os.environ.clear()
    os.environ.update(env)
    version = run(["ansible-playbook", "--version"])
    first = version.stdout.splitlines()[0] if version.stdout else version.stderr.strip()
    if version.returncode or REFERENCE not in first:
        raise Failed(f"ansible-playbook is not {REFERENCE}: {first}")
    mismatched = collection_mismatches()
    if mismatched:
        raise Failed("collections do not match COLLECTIONS: " + "; ".join(mismatched))
