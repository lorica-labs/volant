# SPDX-License-Identifier: GPL-3.0-or-later
from ansible.plugins.action import ActionBase


class ActionModule(ActionBase):
    def run(self, tmp=None, task_vars=None):
        result = super().run(tmp, task_vars)
        result.update(unreachable=True, msg='gone on purpose')
        return result
