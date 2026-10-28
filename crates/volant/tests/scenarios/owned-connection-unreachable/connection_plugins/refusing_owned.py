from ansible.errors import AnsibleConnectionFailure
from ansible.plugins.connection.local import Connection as Local

DOCUMENTATION = """
    name: refusing_owned
    short_description: a connection that always refuses
    description:
        - The local connection, refusing to connect.
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
    transport = "refusing_owned"

    def _connect(self):
        raise AnsibleConnectionFailure("the device said no")
