from ansible.plugins.action import ActionBase


class ActionModule(ActionBase):
    TRANSFERS_FILES = False

    def run(self, tmp=None, task_vars=None):
        result = super().run(tmp, task_vars)
        result["changed"] = False
        result["msg"] = self._task.args.get("msg", "Hello overridden world!")
        return result
