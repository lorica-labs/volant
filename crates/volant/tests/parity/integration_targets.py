"""Run ansible-core's integration targets under the reference and under Volant.

Each target runs the way `ansible-test integration` runs it (`command_integration_role` and
`command_integration_script` in ansible-core 2.19): a role target as a one-play playbook on
`testhost` with `ansible_connection=local`, a script target as its `runme.sh`, both in a fresh
copy of `test/integration` per engine, with ansible-test's environment. Volant runs as the
`ansible-*` program names `volant shims install` links, first on `PATH`.

A target that changes the machine (`destructive`, `needs/root`) and every script target run in a
throwaway container, one per target and engine, as root. The targets this machine cannot run
(`needs/ssh`, `needs/httptester`, `needs/privileged`, `cloud/`, `windows`, `network`) are skipped.

One line per target. The run fails on a target that passes under the reference and fails under
Volant, unless the known file lists it with the issue that tracks it, and on a listed target that
now passes, so the list loses it.

Usage: integration_targets.py ANSIBLE_SRC WORK VOLANT KNOWN [--only NAME ...] [--jobs N]
       [--no-container]
"""

import argparse
import concurrent.futures
import json
import os
import shutil
import subprocess
import sys

IMAGE = "volant-integration:24.04"
TIMEOUT = 20 * 60
SKIPPED = ("needs/ssh", "needs/httptester", "needs/privileged", "cloud/", "windows", "network/")
UNRUN = ("disabled", "unsupported", "hidden", "unstable")


def aliases(target_dir):
    """The target's aliases, comments and blank lines left out."""
    path = os.path.join(target_dir, "aliases")
    if not os.path.exists(path):
        return []
    with open(path) as handle:
        return [line.split("#")[0].strip() for line in handle if line.split("#")[0].strip()]


def kind_of(target_dir, names):
    """`role`, `script` or None for a target this run does not run."""
    if any(name in UNRUN or name.startswith(SKIPPED) for name in names):
        return None
    if os.path.exists(os.path.join(target_dir, "runme.sh")):
        return "script"
    if os.path.exists(os.path.join(target_dir, "tasks", "main.yml")):
        return "role"
    return None


def environment(integration, output_dir, inventory, path_first, python):
    """ansible-test's `ansible_environment` and `integration_environment`, coverage left out."""
    env = {
        "HOME": os.environ.get("HOME", "/root"),
        "LANG": "C.UTF-8",
        "LC_ALL": "C.UTF-8",
        "PATH": path_first + os.pathsep + os.path.dirname(python) + os.pathsep
        + "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin",
        "ANSIBLE_FORCE_COLOR": "false",
        "ANSIBLE_FORCE_HANDLERS": "true",
        "ANSIBLE_HOST_PATTERN_MISMATCH": "error",
        "ANSIBLE_INVENTORY": "/dev/null",
        "ANSIBLE_DEPRECATION_WARNINGS": "false",
        "ANSIBLE_HOST_KEY_CHECKING": "false",
        "ANSIBLE_RETRY_FILES_ENABLED": "false",
        "ANSIBLE_CONFIG": os.path.join(integration, "ansible-test.cfg"),
        "ANSIBLE_LIBRARY": "/dev/null",
        "ANSIBLE_DEVEL_WARNING": "false",
        "ANSIBLE_PYTHON_INTERPRETER": "/set/ansible_python_interpreter/in/inventory",
        "PAGER": "/bin/cat",
        "ANSIBLE_TEST_ANSIBLE_VERSION": "2.19",
        "OUTPUT_DIR": output_dir,
        "INVENTORY_PATH": inventory,
        "ANSIBLE_PLAYBOOK_DIR": integration,
        "ANSIBLE_ROLES_PATH": os.path.join(integration, "targets"),
    }
    return env


def remove(path):
    """`path` and everything under it, a file an earlier run's container left as root included."""
    try:
        shutil.rmtree(path)
    except PermissionError:
        subprocess.run(["docker", "run", "--rm", "-v", "%s:/gone" % path, IMAGE, "sh", "-c",
                        "rm -rf /gone/* /gone/.[!.]*"], check=True)
        shutil.rmtree(path)


def prepare(src, work, engine, python):
    """A fresh copy of `test/integration` for one engine, with its inventory and config."""
    integration = os.path.join(work, engine, "integration")
    if os.path.exists(os.path.join(work, engine)):
        remove(os.path.join(work, engine))
    shutil.copytree(os.path.join(src, "test", "integration"), integration, symlinks=True)
    with open(os.path.join(integration, "inventory"), "w") as handle:
        handle.write(
            "[testgroup]\ntesthost ansible_connection=local ansible_pipelining=yes "
            "ansible_python_interpreter=%s\n" % python
        )
    open(os.path.join(integration, "ansible-test.cfg"), "w").close()
    return integration


def commands(integration, target, names, kind, output_dir):
    """The commands one target runs: its setup targets, then itself, each with its cwd."""
    targets = os.path.join(integration, "targets")
    setups = [n.split("/", 2)[2] for n in names if n.startswith(("setup/once/", "setup/always/"))]
    out = []
    for name in setups + [target]:
        if name != target or kind == "role":
            setup_names = aliases(os.path.join(targets, name))
            facts = "gather_facts/no" not in setup_names
            play = [{"hosts": "testhost", "gather_facts": facts, "vars": {"output_dir": output_dir},
                     "roles": [name]}]
            playbook = os.path.join(integration, "%s--%s-play.json" % (target, name))
            text = json.dumps(play, indent=4, sort_keys=True)
            with open(playbook, "w") as handle:
                handle.write(text)
            out.append((["ansible-playbook", playbook, "-i", "inventory"], integration))
        else:
            out.append((["./runme.sh"], os.path.join(targets, target)))
    return out


def run_one(src, work, engine, bins, python, target, kind, names, container):
    """Pass, fail or timeout for one target under one engine, with the log beside it."""
    integration = os.path.join(work, engine, "integration")
    output_dir = os.path.join(work, engine, "output", target)
    shutil.rmtree(output_dir, ignore_errors=True)
    os.makedirs(output_dir)
    env = environment(integration, output_dir, os.path.join(integration, "inventory"), bins, python)
    if engine == "volant":
        env["VOLANT_PYTHON"] = python
    log = os.path.join(work, engine, "logs", target + ".log")
    os.makedirs(os.path.dirname(log), exist_ok=True)
    status = "pass"
    with open(log, "w") as handle:
        for cmd, cwd in commands(integration, target, names, kind, output_dir):
            if container:
                cmd = docker(cmd, cwd, env, work, bins, python, target, engine)
            try:
                done = subprocess.run(cmd, cwd=cwd, env=None if container else env, stdout=handle,
                                      stderr=subprocess.STDOUT, timeout=TIMEOUT)
            except subprocess.TimeoutExpired:
                status = "timeout"
                break
            if done.returncode != 0:
                status = "fail"
                break
    return status


def docker(cmd, cwd, env, work, bins, python, target, engine):
    """`cmd` in a throwaway container, as root, with the work tree, the engine and the reference's
    interpreter mounted at their own paths."""
    mounts = {os.path.realpath(work), os.path.dirname(os.path.dirname(python))}
    mounts.update(os.path.dirname(os.path.realpath(os.path.join(bins, b))) for b in os.listdir(bins))
    real = os.path.realpath(python)
    mounts.add(os.path.dirname(os.path.dirname(real)))
    for extra in os.environ.get("VOLANT_INTEGRATION_MOUNTS", "").split(os.pathsep):
        if extra:
            mounts.add(extra)
    out = ["docker", "run", "--rm", "--name", "volant-it-%s-%s-%d" % (engine, target, os.getpid()),
           "-w", cwd]
    for mount in sorted(mounts):
        out += ["-v", "%s:%s" % (mount, mount)]
    for key, value in env.items():
        if key != "HOME":
            out += ["-e", "%s=%s" % (key, value)]
    # What the target wrote as root goes back to the user running this, so the next run can remove
    # it: the target's own directory, or the whole copy for a role, and its output directory.
    owner = "%d:%d" % (os.getuid(), os.getgid())
    give_back = '"$@"; rc=$?; chown -R %s "$PWD" "$OUTPUT_DIR"; exit $rc' % owner
    return out + [IMAGE, "sh", "-c", give_back, "sh"] + cmd


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("src")
    parser.add_argument("work")
    parser.add_argument("volant")
    parser.add_argument("known")
    parser.add_argument("--only", nargs="*", default=[])
    parser.add_argument("--jobs", type=int, default=os.cpu_count() or 4)
    parser.add_argument("--no-container", action="store_true")
    args = parser.parse_args()

    python = sys.executable
    reference = os.path.dirname(python)
    work = os.path.realpath(args.work)
    shims = os.path.join(work, "shims")
    os.makedirs(shims, exist_ok=True)
    subprocess.run([args.volant, "shims", "install", "--dir", shims], check=True,
                   stdout=subprocess.DEVNULL)
    engines = {"reference": reference, "volant": shims}
    for engine in engines:
        prepare(args.src, work, engine, python)

    targets_dir = os.path.join(args.src, "test", "integration", "targets")
    plan = []
    for target in sorted(os.listdir(targets_dir)):
        if args.only and target not in args.only:
            continue
        names = aliases(os.path.join(targets_dir, target))
        kind = kind_of(os.path.join(targets_dir, target), names)
        if kind is None:
            continue
        container = kind == "script" or "destructive" in names or "needs/root" in names
        if container and args.no_container:
            continue
        plan.append((target, kind, names, container))

    known = {}
    if os.path.exists(args.known):
        with open(args.known) as handle:
            for line in handle:
                line = line.split("#", 1)
                name = line[0].strip()
                if name:
                    known[name] = line[1].strip() if len(line) > 1 else ""

    results = {}
    with concurrent.futures.ThreadPoolExecutor(args.jobs) as pool:
        futures = {
            pool.submit(run_one, args.src, work, engine, bins, python, target, kind, names, container):
            (target, engine)
            for target, kind, names, container in plan
            for engine, bins in engines.items()
        }
        for future in concurrent.futures.as_completed(futures):
            target, engine = futures[future]
            results.setdefault(target, {})[engine] = future.result()

    new, fixed, counts = [], [], {"both": 0, "differ": 0, "reference fails": 0}
    for target, _, _, container in plan:
        ref, vol = results[target]["reference"], results[target]["volant"]
        where = "container" if container else "host"
        if ref != "pass":
            verdict = "reference fails"
        elif vol == "pass":
            verdict = "both"
        else:
            verdict = "differ"
        counts[verdict] += 1
        note = ""
        if verdict == "differ" and target not in known:
            new.append(target)
            note = "  NEW"
        elif verdict == "differ":
            note = "  #" + known[target]
        elif verdict == "both" and target in known:
            fixed.append(target)
            note = "  FIXED, drop it from the known file"
        print("%-40s %-9s reference=%-7s volant=%-7s%s" % (target, where, ref, vol, note))
    print("%d targets: %d pass under both, %d differ, %d fail under the reference"
          % (len(plan), counts["both"], counts["differ"], counts["reference fails"]))
    print("logs: %s/<engine>/logs/<target>.log" % work)
    if new or fixed:
        print("new differences: %s" % (", ".join(new) or "none"))
        print("listed but passing: %s" % (", ".join(fixed) or "none"))
        sys.exit(1)


if __name__ == "__main__":
    main()
