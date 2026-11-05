"""Python callback plugins for one run of the controller, which sends each event as a frame.

Standard output and standard error are the run's own, so what a plugin prints lands where it
lands under ansible-playbook. The frames travel over the socket given as standard input. Loading
and dispatch are ansible-core's own `TaskQueueManager`, built without what it needs to run tasks.
"""

import json
import os
import socket
import struct
import sys
import threading

TQM = []
PLAYBOOK = []
TASKS = {}
HOSTS = {}


def read_frame(stream):
    header = stream.read(4)
    if len(header) < 4:
        return None
    payload = stream.read(struct.unpack(">I", header)[0])
    return json.loads(payload)


def write_frame(stream, message):
    payload = json.dumps(message).encode()
    stream.write(struct.pack(">I", len(payload)))
    stream.write(payload)
    stream.flush()


def load(request):
    """The run's callbacks, as `TaskQueueManager.load_callbacks` chooses them. The count is what
    the controller keeps the helper for: `stdout` is empty when the controller prints stdout."""
    from ansible import constants as C, context
    from ansible.executor.task_queue_manager import TaskQueueManager
    from ansible.module_utils.common.collections import ImmutableDict
    from ansible.plugins.loader import add_all_plugin_dirs, init_plugin_loader
    from ansible.utils.collection_loader import AnsibleCollectionConfig
    from ansible.utils.display import Display

    context.CLIARGS = ImmutableDict({"connection": C.DEFAULT_TRANSPORT, **request["cliargs"]})
    Display().verbosity = request["cliargs"]["verbosity"]
    init_plugin_loader()
    for directory in request["dirs"]:
        add_all_plugin_dirs(directory)
    AnsibleCollectionConfig.playbook_paths = list(request["dirs"])
    C.CALLBACKS_ENABLED = request["enabled"]
    tqm = TaskQueueManager.__new__(TaskQueueManager)
    tqm._stdout_callback_name = request["stdout"] or "default"
    tqm._run_additional_callbacks = request["additional"]
    tqm._run_tree = False
    tqm._callback_plugins = []
    tqm._callback_lock = threading.Lock()
    tqm.load_callbacks()
    if not request["stdout"]:
        tqm._callback_plugins.pop(0)
    TQM.append(tqm)
    return len(tqm._callback_plugins)


def host(name):
    from ansible.inventory.host import Host

    return HOSTS.setdefault(name, Host(name))


def make_task(fields):
    """A task as a callback reads it. The fields it has no value for keep their defaults."""
    from ansible.playbook.handler import Handler
    from ansible.playbook.task import Task

    task = (Handler if fields.pop("handler", False) else Task)()
    task._uuid = fields.pop("uuid")
    path = fields.pop("path", None) or ""
    task.get_path = lambda: path
    for key, value in fields.items():
        setattr(task, key, value)
    TASKS[task._uuid] = task
    return task


def summaries(data):
    """A result's notes as the objects `CallbackBase._handle_warnings` and `_handle_exception`
    display: the controller keeps them as text."""
    from ansible.module_utils._internal import _messages

    if isinstance(data.get("exception"), str):
        data["exception"] = _messages.ErrorSummary(event=_messages.Event(msg=data["exception"]))
    if data.get("warnings"):
        data["warnings"] = [
            _messages.WarningSummary(event=_messages.Event(msg=str(text))) for text in data["warnings"]
        ]
    if data.get("deprecations"):
        data["deprecations"] = [
            _messages.DeprecationSummary(
                event=_messages.Event(msg=str(note.get("msg"))), version=note.get("version")
            )
            if isinstance(note, dict)
            else _messages.DeprecationSummary(event=_messages.Event(msg=str(note)))
            for note in data["deprecations"]
        ]
    return data


def task_result(request):
    from ansible.executor.task_result import _RawTaskResult

    task = TASKS[request["task"]]
    return _RawTaskResult(host(request["host"]), task, summaries(request["data"]), task.dump_attrs())


def playbook_start(request):
    from ansible.parsing.dataloader import DataLoader
    from ansible.playbook import Playbook

    playbook = Playbook(DataLoader())
    playbook._file_name = request["file"]
    playbook._basedir = os.path.dirname(os.path.abspath(request["file"]))
    PLAYBOOK[:] = [playbook]
    return ("v2_playbook_on_start", (playbook,), {})


def play_start(request):
    from ansible.playbook.play import Play
    from ansible.playbook.play_context import PlayContext

    play = Play()
    for key, value in request["fields"].items():
        setattr(play, key, value)
    if PLAYBOOK:
        PLAYBOOK[0]._entries.append(play)
    context = PlayContext(play=play)
    for plugin in TQM[0]._callback_plugins:
        plugin.set_play_context(context)
    return ("v2_playbook_on_play_start", (play,), {})


def task_start(request):
    task = make_task(request["fields"])
    from ansible.playbook.handler import Handler

    if isinstance(task, Handler):
        return ("v2_playbook_on_handler_task_start", (task,), {})
    return ("v2_playbook_on_task_start", (task,), {"is_conditional": False})


def notify(request):
    from ansible.playbook.handler import Handler

    handler = Handler()
    handler.name = request["handler"]
    return ("v2_playbook_on_notify", (handler, host(request["host"])), {})


def included(request):
    from ansible.playbook.included_file import IncludedFile

    task = TASKS[request["task"]]
    file = IncludedFile(request["file"], {}, {}, task, is_role=request["role"])
    file._hosts = [host(name) for name in request["hosts"]]
    return ("v2_playbook_on_include", (file,), {})


def stats(request):
    from ansible.executor.stats import AggregateStats

    aggregate = AggregateStats()
    for name, counts in request["hosts"].items():
        aggregate.processed[name] = 1
        for field, count in counts.items():
            getattr(aggregate, field)[name] = count
    aggregate.custom = request["custom"]
    return ("v2_playbook_on_stats", (aggregate,), {})


def vars_prompt(request):
    return ("v2_playbook_on_vars_prompt", (), request["fields"])


EVENTS = {
    "playbook": playbook_start,
    "play": play_start,
    "task": task_start,
    "notify": notify,
    "included": included,
    "stats": stats,
    "vars_prompt": vars_prompt,
    "no_hosts_matched": lambda request: ("v2_playbook_on_no_hosts_matched", (), {}),
    "no_hosts_remaining": lambda request: ("v2_playbook_on_no_hosts_remaining", (), {}),
    "runner_start": lambda request: (
        "v2_runner_on_start",
        (host(request["host"]), TASKS[request["task"]]),
        {},
    ),
    "result": lambda request: (request["method"], (task_result(request),), request.get("kwargs", {})),
}


def answer(request):
    if "load" in request:
        try:
            return {"loaded": load(request["load"])}
        except Exception as failure:
            return {"error": str(failure)}
    method, args, kwargs = EVENTS[request["event"]](request)
    TQM[0].send_callback(method, *args, **kwargs)
    return {}


def main():
    channel = socket.socket(fileno=os.dup(0))
    requests = channel.makefile("rb")
    frames = channel.makefile("wb")
    null = os.open(os.devnull, os.O_RDONLY)
    os.dup2(null, 0)
    os.close(null)
    sys.stdin = open(os.devnull)
    sys.path[:] = [entry for entry in sys.path if entry]
    while True:
        request = read_frame(requests)
        if request is None:
            return
        try:
            reply = answer(request)
        except Exception as failure:
            reply = {"error": "%s: %s" % (type(failure).__name__, failure)}
        sys.stdout.flush()
        sys.stderr.flush()
        write_frame(frames, reply)


main()
