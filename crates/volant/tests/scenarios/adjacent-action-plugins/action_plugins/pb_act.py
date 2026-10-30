from ansible.plugins.action import ActionBase


class ActionModule(ActionBase):
    def run(self, tmp=None, task_vars=None):
        result = super().run(tmp, task_vars)
        result['said'] = 'playbook %s' % self._task.args.get('word', '')
        return result
