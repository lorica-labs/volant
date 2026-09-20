# SPDX-License-Identifier: GPL-3.0-or-later
"""One warm Python server per target user, forking a child per module.

Preloads module_utils once from the union zip the controller sent, then forks per task. The
parent never imports a module and never runs module code, so nothing a module does can reach
the next one: measured, without the fork the second module sees the first one's environment,
working directory, sys.path and umask.

Preloading is the whole point: measured, forking alone takes a task from 340 ms to 266 ms, and
preloading takes it to 13 ms. It is safe against a second module only because every module in
the run shares this one union zip - a zip built per module would pin ansible.__path__ to it and
every other module would fail to import.

Frames on stdin and stdout are the agent's own: four bytes of big-endian length, then JSON. One
request in, two frames out - the child's pid as soon as it exists, so the agent can kill it on a
deadline or a cancel, then its result.
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

import json
import os
import selectors
import struct


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


def run_child(request, blob, loader, out_w, err_w):
    """The forked half. Never returns: it exits the process."""
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
        for key, value in request.get("environment", {}).items():
            os.environ[key] = value
        set_open_file_limit(request.get("rlimit_nofile") or 0)
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
        # Without this the module's own result is discarded: os._exit skips the buffer, so a
        # module that printed its result and exited emits nothing at all.
        for stream in (sys.stdout, sys.stderr):
            try:
                stream.flush()
            except Exception:
                pass
        os._exit(code)


def main():
    blob = sys.argv[1]
    sys.path.insert(0, blob)
    from ansible.module_utils._internal._ansiballz import _loader

    # Imported for its cost, not for its name: this is the import every module pays for, and
    # paying it here is what takes a task from 266 ms to 13 ms.
    import ansible.module_utils.basic  # noqa: F401

    stdin = sys.stdin.buffer
    stdout = sys.stdout.buffer
    write_frame(stdout, {"ready": True})

    while True:
        request = read_frame(stdin)
        if request is None:
            return 0
        out_r, out_w = os.pipe()
        err_r, err_w = os.pipe()
        # Flush before forking: measured, a child inherits the parent's unflushed buffer and
        # re-emits it, so the parent's bytes would arrive prefixed to the module's result.
        sys.stdout.flush()
        sys.stderr.flush()
        pid = os.fork()
        if pid == 0:
            os.close(out_r)
            os.close(err_r)
            run_child(request, blob, _loader, out_w, err_w)
        os.close(out_w)
        os.close(err_w)
        try:
            os.setpgid(pid, pid)
        except OSError:
            pass
        # The pid before the result: the agent has a deadline and a cancel to honour, and the
        # result frame only arrives once the module is done - which is exactly when it is too
        # late to kill it.
        write_frame(stdout, {"started": pid})
        streams = drain({"stdout": out_r, "stderr": err_r})
        _, status = os.waitpid(pid, 0)
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
            },
        )


if __name__ == "__main__":
    sys.exit(main())
