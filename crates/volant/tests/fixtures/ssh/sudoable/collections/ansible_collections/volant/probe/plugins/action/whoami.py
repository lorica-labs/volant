# SPDX-License-Identifier: GPL-3.0-or-later
from ansible.plugins.action import ActionBase


class ActionModule(ActionBase):
    def run(self, tmp=None, task_vars=None):
        r = super().run(tmp, task_vars)
        a = self._low_level_execute_command("id -un", sudoable=True)
        b = self._low_level_execute_command("id -un", sudoable=False)
        return dict(r, changed=False, sudoable=a["stdout"].strip(), not_sudoable=b["stdout"].strip())
