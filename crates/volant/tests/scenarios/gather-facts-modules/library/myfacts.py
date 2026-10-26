# SPDX-License-Identifier: GPL-3.0-or-later
from ansible.module_utils.basic import AnsibleModule


def main():
    m = AnsibleModule(argument_spec={"fail": {"type": "bool", "default": False}}, supports_check_mode=True)
    if m.params["fail"]:
        m.fail_json(msg="myfacts broke")
    m.exit_json(ansible_facts={"my_fact": 1})


main()
