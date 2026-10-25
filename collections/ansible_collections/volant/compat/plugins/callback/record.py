# SPDX-License-Identifier: GPL-3.0-or-later
from __future__ import annotations

DOCUMENTATION = r"""
name: record
type: notification
short_description: Record a run for Volant to replay
description:
  - Writes what ansible-core did during a run to a JSON Lines file, one event per line, for
    C(volant shadow replay) to compare against Volant.
  - The recording holds task results as ansible-core saw them, so it can contain decrypted vault
    values and the run's extra variables. It is created with mode 0600. Keep it out of version
    control and treat it as a secret.
  - It never changes the run. When the file cannot be written, it prints a warning and the run goes
    on.
requirements:
  - Enabled in C(callbacks_enabled).
options:
  path:
    description:
      - The file to write. C({{ timestamp }}) is replaced by the time the run started, in UTC, as
        C(%Y%m%dT%H%M%SZ); no other template is read.
      - A name ending in C(.gz) is compressed with gzip.
      - Missing parent directories are created with mode 0700.
    type: str
    default: shadow/run-{{ timestamp }}.jsonl.gz
    ini:
      - section: callback_volant_record
        key: path
    env:
      - name: VOLANT_RECORD_PATH
"""

import atexit
import collections.abc
import datetime
import gzip
import hashlib
import json
import os

from ansible import context
from ansible.plugins.callback import CallbackBase
from ansible.release import __version__ as ansible_version

FORMAT = 1


def _sha256(path):
    digest = hashlib.sha256()
    with open(path, "rb") as file:
        for chunk in iter(lambda: file.read(1 << 16), b""):
            digest.update(chunk)
    return digest.hexdigest()


def _plain(value):
    """What `json` cannot write as it is, in a form it can."""
    if isinstance(value, collections.abc.Mapping):
        return dict(value)
    if isinstance(value, (set, frozenset, tuple, list)):
        return list(value)
    if isinstance(value, bytes):
        return value.decode("utf-8", "replace")
    return str(value)


def _hash(value):
    text = json.dumps(value, default=_plain, sort_keys=True)
    return hashlib.sha256(text.encode()).hexdigest()


class CallbackModule(CallbackBase):
    CALLBACK_VERSION = 2.0
    CALLBACK_TYPE = "notification"
    CALLBACK_NAME = "volant.compat.record"
    CALLBACK_NEEDS_ENABLED = True

    def __init__(self, display=None):
        super().__init__(display)
        self._started = datetime.datetime.now(datetime.timezone.utc)
        self._raw = None
        self._out = None
        self._opened = False
        # The index of the next item of each (host, task) loop.
        self._items = {}
        atexit.register(self._close)

    def _open(self, play):
        self._opened = True
        stamp = self._started.strftime("%Y%m%dT%H%M%SZ")
        path = os.path.expanduser(self.get_option("path").replace("{{ timestamp }}", stamp))
        try:
            parent = os.path.dirname(path)
            if parent:
                os.makedirs(parent, mode=0o700, exist_ok=True)
            fd = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o600)
            os.fchmod(fd, 0o600)
            self._raw = os.fdopen(fd, "wb")
            self._out = gzip.GzipFile(fileobj=self._raw, mode="wb") if path.endswith(".gz") else self._raw
        except OSError as error:
            self._give_up(f"cannot write the recording {path}: {error}")
            return

        cli = dict(context.CLIARGS)
        variables = play.get_variable_manager()
        sources = list(cli.get("inventory") or ())
        self._write(
            "header",
            format=FORMAT,
            ansible_core=ansible_version,
            cli=cli,
            cwd=os.getcwd(),
            playbooks=[{"path": os.path.abspath(p), "sha256": _sha256(p)} for p in cli.get("args") or ()],
            inventory={
                "sources": sources,
                "files": [{"path": os.path.abspath(s), "sha256": _sha256(s)} for s in sources if os.path.isfile(s)],
            },
            extra_vars_hash=_hash(variables.extra_vars),
            started=self._started.isoformat(),
        )
        groups = variables.get_vars(include_hostvars=False)["groups"]
        hosts = {}
        for group, members in groups.items():
            for host in members:
                hosts.setdefault(host, [])
                if group != "all":
                    hosts[host].append(group)
        self._write("inventory", hosts={host: sorted(names) for host, names in hosts.items()})

    def _write(self, event, **fields):
        if self._out is None:
            return
        try:
            line = json.dumps({"event": event, **fields}, default=_plain)
            self._out.write(line.encode() + b"\n")
        except (OSError, TypeError, ValueError) as error:
            self._give_up(f"cannot write the recording: {error}")

    def _give_up(self, why):
        self._display.warning(f"volant.compat.record: {why}; the run goes on without a recording")
        self._close()

    def _close(self):
        out, raw = self._out, self._raw
        self._out = self._raw = None
        for file in (out, raw):
            if file is not None:
                try:
                    file.close()
                except OSError:
                    pass

    def _task(self, task, handler):
        # A templated action is only resolved in the worker, and asking for it here warns.
        action = task.action if "{{" in task.action else task.resolved_action
        self._write(
            "task",
            uuid=task._uuid,
            path=task.get_path(),
            name=task.get_name(),
            action=action,
            handler=handler,
            role=task._role.get_name() if task._role else None,
        )

    def _result(self, result, status=None, item=False):
        host = result.host.get_name()
        task = result.task._uuid
        key = (host, task)
        if item:
            index = self._items[key] = self._items.get(key, -1) + 1
        else:
            index = None
            self._items.pop(key, None)
        data = result.result
        delegated = data.get("_ansible_delegated_vars") or {}
        delegated_to = delegated.get("ansible_delegated_host")
        if delegated and delegated_to is None:
            # Before 2.21 the delegated vars carry only the address; the task names the host.
            delegated_to = result.task_fields.get("delegate_to")
        if status is None:
            status = "changed" if data.get("changed") else "ok"
        if data.get("_ansible_no_log"):
            data = {"censored": True}
        self._write(
            "result",
            host=host,
            task=task,
            item=index,
            status=status,
            delegated_to=delegated_to,
            result=data,
        )

    def v2_playbook_on_play_start(self, play):
        if not self._opened:
            self._open(play)
        self._write("play", uuid=play._uuid, name=play.get_name(), hosts=play.hosts)

    def v2_playbook_on_task_start(self, task, is_conditional):
        self._task(task, handler=False)

    def v2_playbook_on_handler_task_start(self, task):
        self._task(task, handler=True)

    def v2_runner_on_ok(self, result):
        self._result(result)

    def v2_runner_on_failed(self, result, ignore_errors=False):
        self._result(result, "failed")

    def v2_runner_on_skipped(self, result):
        self._result(result, "skipped")

    def v2_runner_on_unreachable(self, result):
        self._result(result, "unreachable")

    def v2_runner_item_on_ok(self, result):
        self._result(result, item=True)

    def v2_runner_item_on_failed(self, result):
        self._result(result, "failed", item=True)

    def v2_runner_item_on_skipped(self, result):
        self._result(result, "skipped", item=True)

    def v2_runner_retry(self, result):
        data = result.result
        self._write(
            "retry",
            host=result.host.get_name(),
            task=result.task._uuid,
            attempt=data.get("attempts"),
            result={"censored": True} if data.get("_ansible_no_log") else data,
        )

    def v2_playbook_on_include(self, included_file):
        self._write(
            "include",
            file=included_file._filename,
            hosts=[host.get_name() for host in included_file._hosts],
        )

    def v2_playbook_on_notify(self, handler, host):
        self._write("notify", handler=handler.get_name(), host=host.get_name())

    def v2_playbook_on_stats(self, stats):
        hosts = {host: stats.summarize(host) for host in sorted(stats.processed)}
        self._write("stats", hosts=hosts, custom=stats.custom)
        self._close()
