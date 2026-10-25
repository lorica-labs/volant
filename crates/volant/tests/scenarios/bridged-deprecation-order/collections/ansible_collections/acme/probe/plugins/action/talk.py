# SPDX-License-Identifier: GPL-3.0-or-later
from ansible.plugins.action import ActionBase
from ansible.utils.display import Display


class ActionModule(ActionBase):
    def run(self, tmp=None, task_vars=None):
        result = super().run(tmp, task_vars)
        Display().deprecated("talk is going away", version="9.0.0", collection_name="acme.probe")
        Display().warning("talk warns")
        result["changed"] = False
        return result
