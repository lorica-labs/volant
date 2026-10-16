# SPDX-License-Identifier: GPL-3.0-or-later
from ansible.module_utils.basic import AnsibleModule
def main():
    m = AnsibleModule(argument_spec={'who': {'default': 'none'}, 'how': {'default': 'none'}})
    m.exit_json(changed=False, who=m.params['who'], how=m.params['how'])
main()
