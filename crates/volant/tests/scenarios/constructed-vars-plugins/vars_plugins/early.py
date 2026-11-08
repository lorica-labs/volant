# SPDX-License-Identifier: GPL-3.0-or-later
import os

from ansible.inventory.host import Host
from ansible.plugins.vars import BaseVarsPlugin


class VarsModule(BaseVarsPlugin):
    def get_vars(self, loader, path, entities, cache=True):
        out = {}
        for entity in entities:
            if isinstance(entity, Host):
                out["clash_host"] = "early"
                out["early_tier"] = "gold" if entity.name == "h1" else "silver"
            else:
                out["clash_group"] = "early:" + entity.name
                out["early_note"] = "early:" + os.path.basename(path)
        return out
