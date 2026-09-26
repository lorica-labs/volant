# SPDX-License-Identifier: GPL-3.0-or-later
"""The report `natives()` records and golden.rs replays against, without `ansible.posix`.

ansible-core 2.19 routes `ansible.builtin.json` to `ansible.posix.json`, a collection a bare
ansible-core install does not have. This keeps what that callback keeps of a task (the module's
result, `failed` or `skipped` when so, the task's `action`) in the same `plays[0].tasks[]` shape,
one entry per task and host, in the order the results arrive.
"""
import json

from ansible.parsing.ajson import AnsibleJSONEncoder
from ansible.plugins.callback import CallbackBase


class CallbackModule(CallbackBase):
    CALLBACK_VERSION = 2.0
    CALLBACK_TYPE = "stdout"
    CALLBACK_NAME = "golden_json"

    def __init__(self, *args, **kwargs):
        super().__init__(*args, **kwargs)
        self.tasks = []

    def _record(self, result, **info):
        outcome = dict(result._result, **info)
        outcome["action"] = result._task.action
        self.tasks.append({"task": {"name": result._task.get_name()}, "hosts": {result._host.get_name(): outcome}})

    def v2_runner_on_ok(self, result, **kwargs):
        self._record(result)

    def v2_runner_on_failed(self, result, **kwargs):
        self._record(result, failed=True)

    def v2_runner_on_skipped(self, result, **kwargs):
        self._record(result, skipped=True)

    def v2_runner_on_unreachable(self, result, **kwargs):
        self._record(result)

    def v2_playbook_on_stats(self, stats):
        self._display.display(json.dumps({"plays": [{"tasks": self.tasks}]}, cls=AnsibleJSONEncoder, sort_keys=True))
