#!/usr/bin/python
# SPDX-License-Identifier: GPL-3.0-or-later
# The module the `whoami` action plugin stands in front of: the plugin never runs it.
from ansible.module_utils.basic import AnsibleModule


def main():
    AnsibleModule(argument_spec={}).exit_json(changed=False)


if __name__ == "__main__":
    main()
