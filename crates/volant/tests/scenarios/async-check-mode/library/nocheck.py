#!/usr/bin/python
from ansible.module_utils.basic import AnsibleModule

module = AnsibleModule(argument_spec={}, supports_check_mode=False)
module.exit_json(changed=True)
