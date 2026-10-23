# SPDX-License-Identifier: GPL-3.0-or-later
from ansible.errors import AnsibleError
from ansible.plugins.action import ActionBase
from ansible.utils.display import Display


def read(hostvars, host, name):
    try:
        value = hostvars[host][name]
    except Exception as failure:
        return {"error": type(failure).__name__, "msg": str(failure)}
    if isinstance(value, (str, int)):
        return {"value": value}
    return {"type": type(value).__name__}


class ActionModule(ActionBase):
    def run(self, tmp=None, task_vars=None):
        result = super().run(tmp, task_vars)
        what = self._task.args.get("what")
        hostvars = task_vars["hostvars"]
        if what == "hostvars":
            display = Display()
            display.warning("warning from the plugin")
            display.deprecated("deprecated from the plugin", version="9.99")
            display.warning("warning from the plugin")
            display.warning("second warning from the plugin")
            result.update(
                v=read(hostvars, "h2", "v"),
                kind=type(hostvars["h2"]).__name__,
                bad=read(hostvars, "h2", "bad"),
                looked_up=read(hostvars, "h2", "looked_up"),
                unknown=read(hostvars, "nowhere", "v"),
                listed="nowhere" in hostvars,
            )
        elif what == "needle":
            try:
                result["found"] = self._find_needle("files", "outer.txt")
            except AnsibleError as failure:
                result["not_found"] = str(failure)
        elif what == "redirect":
            result.update(self._execute_module(task_vars=task_vars))
        return result
