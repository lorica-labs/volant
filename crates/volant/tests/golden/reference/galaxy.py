# SPDX-License-Identifier: GPL-3.0-or-later
"""The `galaxy` kind: every `galaxy/<name>/case.yml`, run by `ansible-galaxy` against the fake Galaxy
server of galaxy_server.py, recorded in `expected.json` beside it.

case.yml holds `args`, the argv after `ansible-galaxy`, where `<golden-server>` stands for the
server's URL and `<golden-tmp>` for the run's scratch directory. Every other file of the case's
directory but `expected.json` is copied into the scratch directory, where the command runs, so
`-r requirements.yml` names one of them; those copies are left out of the recorded tree. `setup`, a
list of argvs in the same form, runs first and is not recorded, for a case that needs something
installed already. `repos`, a list of `{path, sources, tags}`, makes a git repository at `path` in
the scratch directory: one commit per directory of `sources`, in order, each with the same author
and date so its hash never changes, and `tags` on the first. The reference runs with a `git` that
prints nothing, as Volant shows `git`'s output only at `-vvv`. The run's home is
`<golden-tmp>/home`, so no cache or configuration outlives it, and no `ANSIBLE_*` setting of the
caller is kept.

`expected.json` holds the argv, the exit code, stdout, stderr, the requests the server saw in
order, and every file left under the scratch directory outside the home, with the SHA-256 of its
normalised text, or of its members for a `.tar.gz` (see `members`). Normalised means: the scratch directory reads `<golden-tmp>`, the server's port
`<golden-port>`, ansible-galaxy's own download directory `<golden-galaxy-tmp>` with the random
suffix of the file it downloads to `<golden-random>`, a role's `install_date` `<golden-date>`, and the Python objects after the resolver's `use --pre` hint
are cut, ending it with a full stop.
"""
import hashlib
import io
import os
import re
import shutil
import subprocess
import tarfile

import yaml

from . import GOLDEN
from .galaxy_server import Server
from .redact import TMP
from .run import Failed, run, scratch, write_json

CASES = os.path.join(GOLDEN, "galaxy")
SERVER = "<golden-server>"
PORT = "<golden-port>"
KEYS = {"args", "setup", "repos"}
# `Downloading <url> to <path>`: the path is a temporary directory under the home, and the file in
# it ends with a random suffix.
GALAXY_TMP = re.compile(r"\S*/\.ansible/tmp/ansible-local-[^/\s]+/tmp[^/\s]+/(\S+?)-[a-z0-9_]{8}(?=\s|$)", re.M)
INSTALL_DATE = re.compile(r"^install_date: '[^'\n]*'$", re.M)
# The resolver's hint ends with a dump of Python objects, which Volant does not reproduce.
PRE_HINT = re.compile(r"(use --pre): .*$", re.M)
# The path of the `git` a failed command ran, which differs between machines.
GIT_COMMAND = re.compile(r"Command '([\[(])'[^']*/git', ")


def normalise(text, tmp, port):
    text = text.replace(tmp, TMP).replace(f"127.0.0.1:{port}", f"127.0.0.1:{PORT}")
    text = GALAXY_TMP.sub(r"<golden-galaxy-tmp>/\1-<golden-random>", text)
    text = PRE_HINT.sub(r"\1.", text)
    text = GIT_COMMAND.sub(r"Command '\1'git', ", text)
    return INSTALL_DATE.sub("install_date: '<golden-date>'", text)


def inputs(directory):
    """The files of a case's directory that its run reads."""
    return sorted(n for n in os.listdir(directory) if n not in ("case.yml", "expected.json"))


def members(data):
    """A tarball as its members, sorted by name: name, kind and content hash, a line each. The
    times, owners and compression of the archive itself do not count: ansible-galaxy stamps a
    build with the current time, and zlib's output differs between platforms."""
    lines = []
    with tarfile.open(fileobj=io.BytesIO(data)) as tar:
        for member in sorted(tar.getmembers(), key=lambda m: m.name):
            if member.isdir():
                lines.append(f"{member.name} dir")
            elif member.issym():
                lines.append(f"{member.name} link {member.linkname}")
            else:
                content = tar.extractfile(member).read()
                lines.append(f"{member.name} file {hashlib.sha256(content).hexdigest()}")
    return "\n".join(lines).encode()


def tree(tmp, port, skip=()):
    """Every file under `tmp` outside the home and `skip`, by path, with the SHA-256 of its
    normalised text."""
    files = {}
    for root, dirs, names in os.walk(tmp):
        if root == tmp:
            dirs[:] = [d for d in dirs if d != "home" and d not in skip]
            names = [n for n in names if n not in skip]
        dirs.sort()
        for name in sorted(names):
            path = os.path.join(root, name)
            with open(path, "rb") as f:
                data = f.read()
            if name.endswith(".tar.gz"):
                data = members(data)
            try:
                data = normalise(data.decode("utf-8"), tmp, port).encode("utf-8")
            except UnicodeDecodeError:
                pass
            files[os.path.relpath(path, tmp).replace(os.sep, "/")] = hashlib.sha256(data).hexdigest()
    return files


# Every commit of a fixture repository: the same author and date make the same hashes.
GIT_ENV = {
    "GIT_AUTHOR_NAME": "Volant",
    "GIT_AUTHOR_EMAIL": "volant@example.invalid",
    "GIT_AUTHOR_DATE": "2026-01-01T00:00:00+00:00",
    "GIT_COMMITTER_NAME": "Volant",
    "GIT_COMMITTER_EMAIL": "volant@example.invalid",
    "GIT_COMMITTER_DATE": "2026-01-01T00:00:00+00:00",
}


def repositories(case, tmp):
    """Makes the case's `repos` and returns their paths."""
    made = []
    env = dict(os.environ, **GIT_ENV)
    for repo in case.get("repos") or []:
        path = os.path.join(tmp, repo["path"])
        git = lambda *args, cwd=path: subprocess.run(["git", *args], cwd=cwd, env=env, check=True, capture_output=True)
        os.makedirs(path)
        git("init", "-q", "-b", "main")
        for number, source in enumerate(repo["sources"]):
            for name in os.listdir(path):
                if name != ".git":
                    full = os.path.join(path, name)
                    shutil.rmtree(full) if os.path.isdir(full) else os.remove(full)
            shutil.copytree(os.path.join(tmp, source), path, dirs_exist_ok=True)
            # The index would trust a file of the same size and time as the one it replaces.
            git("rm", "-r", "-q", "--cached", "--ignore-unmatch", ".")
            git("add", "-A")
            git("commit", "-q", "-m", f"fixture {number}")
            if number == 0:
                for tag in repo.get("tags") or []:
                    git("tag", tag)
        made.append(repo["path"].split("/")[0])
    return made


def quiet_git(tmp):
    """A directory holding a `git` that runs the real one and prints nothing."""
    real = shutil.which("git")
    directory = os.path.join(tmp, ".quiet-git")
    os.makedirs(directory)
    script = os.path.join(directory, "git")
    with open(script, "w", encoding="utf-8") as f:
        f.write(f'#!/bin/sh\nexec "{real}" "$@" >/dev/null 2>&1\n')
    os.chmod(script, 0o755)
    return directory


def record():
    names = sorted(n for n in os.listdir(CASES) if os.path.isfile(os.path.join(CASES, n, "case.yml")))
    if not names:
        raise Failed(f"no galaxy case under {CASES}")
    for name in names:
        directory = os.path.join(CASES, name)
        with open(os.path.join(directory, "case.yml"), encoding="utf-8") as f:
            case = yaml.safe_load(f)
        unknown = set(case) - KEYS
        if unknown or not isinstance(case.get("args"), list):
            raise Failed(f"galaxy case {name}: case.yml takes `args`, a list, and `setup`, a list of them")
        with scratch() as tmp, Server() as server:
            home = os.path.join(tmp, "home")
            os.makedirs(home)
            given = inputs(directory)
            for name in given:
                source = os.path.join(directory, name)
                copy = shutil.copytree if os.path.isdir(source) else shutil.copy
                copy(source, os.path.join(tmp, name))
            url = f"http://127.0.0.1:{server.port}/"

            def command(args):
                return ["ansible-galaxy"] + [a.replace(SERVER, url).replace(TMP, tmp) for a in args]

            given += repositories(case, tmp)
            path = quiet_git(tmp) + os.pathsep + os.environ.get("PATH", "")
            given.append(".quiet-git")
            for args in case.get("setup") or []:
                if run(command(args), cwd=tmp, extra_env={"HOME": home, "PATH": path}, forward=()).returncode:
                    raise Failed(f"galaxy case {name}: a setup run failed: {args}")
            del server.log[:]
            argv = command(case["args"])
            done = run(argv, cwd=tmp, extra_env={"HOME": home, "PATH": path}, forward=())
            write_json(os.path.join(directory, "expected.json"), {
                "args": case["args"],
                "setup": case.get("setup") or [],
                "repos": case.get("repos") or [],
                "rc": done.returncode,
                "stdout": normalise(done.stdout, tmp, server.port),
                "stderr": normalise(done.stderr, tmp, server.port),
                "requests": list(server.log),
                "tree": tree(tmp, server.port, given),
            })
