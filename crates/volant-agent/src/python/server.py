# SPDX-License-Identifier: GPL-3.0-or-later
"""One warm Python server per target user, forking a child per module.

Preloads module_utils once from the union zip the controller sent, then forks per task. The
parent never runs a module's own code, so nothing a module does can reach the next one: measured,
without the fork the second module sees the first one's environment, working directory, sys.path
and umask. Once a module's child is running, the parent imports what that module imports at its
top level (see `preimport`), so the module's next child finds it done.

Preloading is the whole point: measured, forking alone takes a task from 340 ms to 266 ms, and
preloading takes it to 13 ms. It is safe against a second module only because every module in
the run shares this one union zip - a zip built per module would pin ansible.__path__ to it and
every other module would fail to import.

Frames on stdin and stdout are the agent's own: four bytes of big-endian length, then JSON. One
request in, two frames out - the child's pid as soon as it exists, so the agent can kill it on a
deadline or a cancel, then its result. The result frame carries the child's own timing, which it
writes on a third pipe just before it exits.
"""

import sys

# `python -c` puts '' at the head of sys.path, which resolves to the agent's working directory
# at every import - and under `become` that is the unprivileged login user's home. A file named
# `selectors.py` there would run as root before this server ever reads the payload. Dropped
# before anything but `sys` is imported, because `sys` is built in and cannot be shadowed. `-P`
# would do it in one flag and is 3.11 upward; `-I` would also remove user site-packages, which
# real modules import from.
if sys.path and sys.path[0] in ("", "."):
    del sys.path[0]

import ast
import importlib.util
import json
import os
import selectors
import struct
import time


# What one child may put in one frame, per stream. The agent refuses a frame over 64 MiB and JSON
# escaping can double what goes into one, so a module returning more than this is failed as an
# oversized result rather than killing the server that carried it.
STREAM_LIMIT = 4 * 1024 * 1024


def set_open_file_limit(wanted):
    """Raises the soft limit on open files to what the payload asked for.

    Only upward, and never past the hard limit: the value exists for modules that need more
    descriptors than the login shell grants, and lowering one a host deliberately raised would be
    a difference nobody asked for. A refusal is not fatal - the module is the better judge of
    whether it can work without it.
    """
    if not wanted:
        return
    try:
        import resource

        soft, hard = resource.getrlimit(resource.RLIMIT_NOFILE)
        if hard != resource.RLIM_INFINITY:
            wanted = min(wanted, hard)
        if soft == resource.RLIM_INFINITY or wanted <= soft:
            return
        resource.setrlimit(resource.RLIMIT_NOFILE, (wanted, hard))
    except (ImportError, ValueError, OSError):
        pass


def read_frame(stream):
    head = stream.read(4)
    if len(head) < 4:
        return None
    (length,) = struct.unpack(">I", head)
    body = stream.read(length)
    if len(body) < length:
        return None
    return json.loads(body)


def write_frame(stream, obj):
    body = json.dumps(obj).encode()
    stream.write(struct.pack(">I", len(body)))
    stream.write(body)
    stream.flush()


def drain(pipes):
    """Everything the child writes, reading every pipe as it fills.

    One pipe at a time would deadlock the pair: a child that fills its stderr buffer while the
    parent is blocked on stdout never gets to the write that would end the read.
    """
    selector = selectors.DefaultSelector()
    readers = {}
    kept = {}
    for name, fd in pipes.items():
        readers[name] = []
        kept[name] = 0
        selector.register(fd, selectors.EVENT_READ, name)
    open_count = len(pipes)
    while open_count:
        for key, _ in selector.select():
            chunk = os.read(key.fileobj, 65536)
            if chunk:
                # Read to the end whatever happens - stopping early would block the child on a
                # full pipe - but stop keeping what could not fit in a frame. A module returning
                # more than this fails as an oversized result rather than taking the server down.
                room = STREAM_LIMIT - kept[key.data]
                if room > 0:
                    readers[key.data].append(chunk[:room])
                    kept[key.data] += min(room, len(chunk))
            else:
                selector.unregister(key.fileobj)
                os.close(key.fileobj)
                open_count -= 1
    selector.close()
    return dict(
        (
            name,
            {
                "text": b"".join(chunks).decode("utf-8", "replace"),
                "truncated": kept[name] >= STREAM_LIMIT,
            },
        )
        for name, chunks in readers.items()
    )


def timed_module_init(basic, marks):
    """Notes when the module builds its `AnsibleModule`: the end of its import and the start of
    its own work.

    Timing the import on its own, by importing the module before `run_module`, would compile
    and run the module's body twice, and `runpy` warns on stderr about a module already imported.
    Measured on the development machine, compiling `user` alone takes 28 ms.

    The mark is taken in `__new__`, which has returned before `__init__` starts: a wrapper around
    `__init__` would add its own frame to the traceback of any exception `__init__` raises, and
    that traceback is what an operator reads in the task's message.
    """
    cls = getattr(basic, "AnsibleModule", None)
    if cls is None or "__new__" in vars(cls):
        return

    def timed(klass, *args, **kwargs):
        if not marks:
            marks.append(time.monotonic_ns())
        return object.__new__(klass)

    cls.__new__ = staticmethod(timed)


def child_timing(forked, started, running, marks, ended):
    """Microseconds for the fork, the import and the module; `None` for what was not reached."""

    def us(start, end):
        return None if start is None or end is None else max(0, (end - start) // 1000)

    built = marks[0] if marks else None
    return {
        "fork_us": us(forked, started),
        "import_us": us(running, built),
        "module_us": us(built if built is not None else running, ended),
    }


def read_timing(fd):
    """What the child wrote on its timing pipe, read once it has exited.

    Non-blocking, and never to the end of the pipe: a process the module left behind can hold
    the write end open for as long as it lives, and waiting for it would hang the task.
    """
    try:
        os.set_blocking(fd, False)
        return json.loads(os.read(fd, 4096))
    except (OSError, ValueError):
        return None
    finally:
        os.close(fd)


def top_level_imports(tree):
    """The import statements a module runs when it is loaded: those outside any function or class
    body, including the ones under an `if` or a `try`."""
    pending = list(tree.body)
    while pending:
        node = pending.pop(0)
        if isinstance(node, (ast.Import, ast.ImportFrom)):
            yield node
        elif not isinstance(node, (ast.FunctionDef, ast.AsyncFunctionDef, ast.ClassDef)):
            pending[:0] = [
                child
                for child in ast.iter_child_nodes(node)
                if isinstance(child, (ast.stmt, ast.excepthandler))
            ]


def preimport(module_fqn):
    """Imports, in this parent, what `module_fqn` imports at its top level, so the children of its
    later tasks find it done. Measured, `systemd_service` spends 207 ms of its 229 importing.

    Called once the child running the module has been forked: that child imports as it always
    did, and only the next one gains. The module's own code is not run here - it is read and its
    import statements are run one by one - so a module without an `if __name__ == "__main__"`
    guard does not act on the host a second time, without its arguments.

    What stays behind is what the imports put in `sys.modules`. The process state an import can
    change - environment, working directory, umask, `sys.path`, the standard streams - is put
    back, so one module's imports cannot reach another module's task. The imports see /dev/null
    as their standard streams, as the child does: this parent's own stdin and stdout carry the
    agent's frames, and a word printed here would break them.

    Any failure is left to the child that needs the import, which fails with its own message;
    here it only leaves the module cold.

    Returns the module's spec, for `KnownSpec`, or `None` when it could not be found.
    """
    spec = None
    environ = os.environ.copy()
    path = list(sys.path)
    streams = (sys.stdin, sys.stdout, sys.stderr)
    mask = os.umask(0o022)
    os.umask(mask)
    cwd = os.open(".", getattr(os, "O_PATH", os.O_RDONLY))
    sys.stdout.flush()
    sys.stderr.flush()
    saved = [os.dup(fd) for fd in (0, 1, 2)]
    null = os.open(os.devnull, os.O_RDWR)
    for fd in (0, 1, 2):
        os.dup2(null, fd)
    os.close(null)
    try:
        spec = importlib.util.find_spec(module_fqn)
        tree = ast.parse(spec.loader.get_source(module_fqn))
        where = {"__name__": module_fqn, "__package__": module_fqn.rpartition(".")[0]}
        for node in top_level_imports(tree):
            try:
                if isinstance(node, ast.Import):
                    for alias in node.names:
                        __import__(alias.name)
                else:
                    names = [alias.name for alias in node.names]
                    __import__(node.module or "", where, None, names, node.level)
            except BaseException:
                pass
    except BaseException:
        pass
    finally:
        for stream in (sys.stdout, sys.stderr) + streams[1:]:
            try:
                stream.flush()
            except Exception:
                pass
        for fd, copy in enumerate(saved):
            os.dup2(copy, fd)
            os.close(copy)
        sys.stdin, sys.stdout, sys.stderr = streams
        os.environ.clear()
        os.environ.update(environ)
        sys.path[:] = path
        os.umask(mask)
        os.fchdir(cwd)
        os.close(cwd)
    return spec


class KnownSpec:
    """A finder that answers once, for the module about to run, with the spec the parent found for
    it, and takes itself off `sys.meta_path` as it does.

    Python 3.12's zipimport compiles a module's whole source just to name its file when it builds
    a spec, and `runpy` asks for one in every child: measured on `target`, `file.py` was compiled
    twice per task, 7.4 ms of 31. The parent already built the spec while it imported the module's
    dependencies, from the same payload, which is named by its hash and never changes under it.
    """

    def __init__(self, name, spec):
        self.name = name
        self.spec = spec

    def find_spec(self, name, path=None, target=None):
        if name != self.name:
            return None
        sys.meta_path.remove(self)
        return self.spec


def fingerprint(blob):
    """The modification time of every entry on `sys.path`: what Python itself watches to see a
    module appear, and what installing, upgrading or removing a package changes. The payload is
    left out: it is named by its hash, which the agent checks before every task."""
    marks = []
    for entry in sys.path:
        if entry == blob:
            continue
        try:
            marks.append(os.stat(entry).st_mtime_ns)
        except OSError:
            marks.append(None)
    return marks


def run_child(request, blob, loader, basic, spec, out_w, err_w, timing_w, forked):
    """The forked half. Never returns: it exits the process."""
    started = time.monotonic_ns()
    running = None
    marks = []
    code = 0
    try:
        # Its own process group, so the agent can kill the module and everything it started
        # without touching this server.
        try:
            os.setpgid(0, 0)
        except OSError:
            pass
        # Standard input is the agent's request pipe until this line. A module that reads it -
        # or anything it starts: git asking for a passphrase, apt reaching debconf - would block
        # for ever on a pipe nobody writes to, and with no `timeout:` the batch hangs with it.
        # The native path hands its own children the same empty stdin.
        os.dup2(os.open(os.devnull, os.O_RDONLY), 0)
        os.dup2(out_w, 1)
        os.dup2(err_w, 2)
        # The originals go: a daemon the module forks (`fork_process` points 0-2 at /dev/null
        # and closes nothing else) would otherwise hold them open, and `drain` would wait for it.
        os.close(out_w)
        os.close(err_w)
        for key, value in request.get("environment", {}).items():
            os.environ[key] = value
        set_open_file_limit(request.get("rlimit_nofile") or 0)
        timed_module_init(basic, marks)
        if spec is not None:
            sys.meta_path.insert(0, KnownSpec(request["module_fqn"], spec))
        running = time.monotonic_ns()
        loader.run_module(
            json_params=json.dumps({"ANSIBLE_MODULE_ARGS": request["args"]}).encode(),
            profile=request["profile"],
            module_fqn=request["module_fqn"],
            modlib_path=blob,
            extensions=request.get("extensions", {}),
        )
    except SystemExit as exit_:
        # What the interpreter itself does with each form: an integer is the status, no argument
        # is 0, and anything else is a message printed to stderr with status 1. Reading a message
        # as status 0 drops the one sentence the module chose to die with.
        if isinstance(exit_.code, int):
            code = exit_.code
        elif exit_.code is None:
            code = 0
        else:
            print(exit_.code, file=sys.stderr)
            code = 1
    except BaseException:
        import traceback

        traceback.print_exc(file=sys.stderr)
        code = 99
    finally:
        ended = time.monotonic_ns()
        # Without this the module's own result is discarded: os._exit skips the buffer, so a
        # module that printed its result and exited emits nothing at all.
        for stream in (sys.stdout, sys.stderr):
            try:
                stream.flush()
            except Exception:
                pass
        # A few dozen bytes into an empty pipe: never blocks, and a timing lost is only that.
        try:
            os.write(
                timing_w,
                json.dumps(child_timing(forked, started, running, marks, ended)).encode(),
            )
        except Exception:
            pass
        os._exit(code)


def main():
    blob = sys.argv[1]
    sys.path.insert(0, blob)
    from ansible.module_utils._internal._ansiballz import _loader

    # Imported for its cost: this is the import every module pays for, and paying it here is what
    # takes a task from 266 ms to 13 ms. The child also times the module's import against it.
    import ansible.module_utils.basic as basic

    stdin = sys.stdin.buffer
    stdout = sys.stdout.buffer
    write_frame(stdout, {"ready": True})
    preimported = set()
    specs = {}
    baseline = fingerprint(blob)

    while True:
        request = read_frame(stdin)
        if request is None:
            return 0
        # The reference's next module starts from nothing and sees a package a task has just
        # installed, upgraded or removed; a child of this parent, once it has imported something,
        # would get the import as it was - a `HAS_X = False` a module_util worked out before X was
        # installed. So a parent whose path has moved asks to be replaced, and the agent starts a
        # fresh one for this request.
        if preimported and fingerprint(blob) != baseline:
            write_frame(stdout, {"stale": True})
            return 0
        out_r, out_w = os.pipe()
        err_r, err_w = os.pipe()
        timing_r, timing_w = os.pipe()
        # Flush before forking: measured, a child inherits the parent's unflushed buffer and
        # re-emits it, so the parent's bytes would arrive prefixed to the module's result.
        sys.stdout.flush()
        sys.stderr.flush()
        forked = time.monotonic_ns()
        pid = os.fork()
        if pid == 0:
            os.close(out_r)
            os.close(err_r)
            os.close(timing_r)
            spec = specs.get(request["module_fqn"])
            run_child(request, blob, _loader, basic, spec, out_w, err_w, timing_w, forked)
        os.close(out_w)
        os.close(err_w)
        os.close(timing_w)
        try:
            os.setpgid(pid, pid)
        except OSError:
            pass
        # The pid before the result: the agent has a deadline and a cancel to honour, and the
        # result frame only arrives once the module is done - which is exactly when it is too
        # late to kill it.
        write_frame(stdout, {"started": pid})
        # After the fork, while the child runs: the child imports as it did, the next one gains.
        if request["module_fqn"] not in preimported:
            preimported.add(request["module_fqn"])
            spec = preimport(request["module_fqn"])
            if spec is not None:
                specs[request["module_fqn"]] = spec
        streams = drain({"stdout": out_r, "stderr": err_r})
        _, status = os.waitpid(pid, 0)
        timing = read_timing(timing_r)
        write_frame(
            stdout,
            {
                "pid": pid,
                "exit": os.WEXITSTATUS(status) if os.WIFEXITED(status) else None,
                "signal": os.WTERMSIG(status) if os.WIFSIGNALED(status) else None,
                "stdout": streams["stdout"]["text"],
                "stderr": streams["stderr"]["text"],
                "truncated": streams["stdout"]["truncated"]
                or streams["stderr"]["truncated"],
                # Sent so the agent's sentence quotes the bound applied here, not a copy of it.
                "limit": STREAM_LIMIT,
                "timing": timing,
            },
        )


if __name__ == "__main__":
    sys.exit(main())
