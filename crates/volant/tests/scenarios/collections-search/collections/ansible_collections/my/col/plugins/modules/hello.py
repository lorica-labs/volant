#!/usr/bin/python
from ansible.module_utils.basic import AnsibleModule
def main():
    m = AnsibleModule(argument_spec={})
    m.exit_json(changed=False, who="col.hello")
main()
