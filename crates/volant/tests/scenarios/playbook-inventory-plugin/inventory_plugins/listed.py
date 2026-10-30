# SPDX-License-Identifier: GPL-3.0-or-later
from __future__ import annotations

from ansible.plugins.inventory import BaseInventoryPlugin


class InventoryModule(BaseInventoryPlugin):
    NAME = 'listed'

    def verify_file(self, path):
        return path.endswith('.listed')

    def parse(self, inventory, loader, path, cache=True):
        super().parse(inventory, loader, path, cache)
        self.inventory.add_group('listed')
        with open(path) as source:
            for line in source:
                name = line.strip()
                if name:
                    self.inventory.add_host(name, group='listed')
                    self.inventory.set_variable(name, 'ansible_connection', 'local')
