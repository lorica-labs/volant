# SPDX-License-Identifier: GPL-3.0-or-later
#!/usr/bin/python
from ansible.module_utils.basic import AnsibleModule


def main():
    module = AnsibleModule(argument_spec={})
    module.exit_json(changed=False, hello="custom")


if __name__ == "__main__":
    main()
