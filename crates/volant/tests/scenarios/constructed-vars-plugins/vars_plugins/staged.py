# SPDX-License-Identifier: GPL-3.0-or-later
DOCUMENTATION = """
    name: staged
    short_description: answers at the inventory stage only
    description: answers at the inventory stage only
    options:
      stage:
        default: inventory
        choices: [all, task, inventory]
"""

from ansible.plugins.vars import BaseVarsPlugin


class VarsModule(BaseVarsPlugin):
    def get_vars(self, loader, path, entities, cache=True):
        return {"staged_note": "staged"}
