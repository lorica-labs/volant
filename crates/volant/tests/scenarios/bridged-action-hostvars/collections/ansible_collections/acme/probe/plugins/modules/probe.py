#!/usr/bin/python
# SPDX-License-Identifier: GPL-3.0-or-later
from ansible.module_utils.basic import AnsibleModule


def main():
    module = AnsibleModule(argument_spec={"what": {"type": "str"}})
    module.warn("warning from the module")
    module.exit_json(changed=False, ran="acme.probe.probe")


if __name__ == "__main__":
    main()
