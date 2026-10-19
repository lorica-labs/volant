# SPDX-License-Identifier: GPL-3.0-or-later
#!/usr/bin/python
from ansible.module_utils.basic import AnsibleModule


def main():
    module = AnsibleModule(argument_spec={})
    module.exit_json(changed=False, wide=2**1100 - 1, mid=2**100 + 1, low=-(2**200), small=7)


if __name__ == "__main__":
    main()
