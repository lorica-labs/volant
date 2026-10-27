from ansible.plugins.connection.local import Connection as Local

DOCUMENTATION = """
    name: fake_owned
    short_description: local under another name
    description:
        - The local connection under another name.
    author: volant
    options:
        become_success_timeout:
            description: As local.
            type: int
            default: 10
        become_strip_preamble:
            description: As local.
            type: bool
            default: true
    extends_documentation_fragment:
        - connection_pipelining
"""


class Connection(Local):
    transport = "fake_owned"
