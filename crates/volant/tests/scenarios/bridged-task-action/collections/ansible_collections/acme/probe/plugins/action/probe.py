# SPDX-License-Identifier: GPL-3.0-or-later
from ansible.plugins.action import ActionBase


class ActionModule(ActionBase):
    def run(self, tmp=None, task_vars=None):
        result = super().run(tmp, task_vars)
        ran = self._execute_module(task_vars=task_vars)
        result.update(action=self._task.action, ran=ran.get("module_name"))
        return result
