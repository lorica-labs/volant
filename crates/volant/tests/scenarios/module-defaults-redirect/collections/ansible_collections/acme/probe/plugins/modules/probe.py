# SPDX-License-Identifier: GPL-3.0-or-later
from ansible.module_utils.basic import AnsibleModule


def main():
    module = AnsibleModule(argument_spec=dict(x=dict(type="int"), y=dict(type="str")))
    module.exit_json(changed=False, got=module.params)


main()
