from ansible.plugins.action import ActionBase


class ActionModule(ActionBase):
    def run(self, tmp=None, task_vars=None):
        result = super().run(tmp, task_vars)
        out = self._low_level_execute_command('echo from-role')
        result['said'] = out['stdout'].strip()
        result['changed'] = True
        return result
