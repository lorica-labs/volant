# SPDX-License-Identifier: GPL-3.0-or-later
from ansible.module_utils.basic import AnsibleModule


def main():
    module = AnsibleModule(argument_spec={})
    module.exit_json(changed=False, module_name=module._name)


main()
