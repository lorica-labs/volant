#!/usr/bin/python
from ansible.module_utils.basic import AnsibleModule

m = AnsibleModule(argument_spec=dict(
    foo=dict(type='str', aliases=['old'], deprecated_aliases=[dict(name='old', version='9.99', collection_name='ansible.builtin')]),
))
m.deprecate('dep message', version='2.99', collection_name='ansible.builtin')
m.warn('a warning')
m.exit_json(changed=False)
