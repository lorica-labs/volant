from ansible.inventory.host import Host
from ansible.plugins.vars import BaseVarsPlugin


class VarsModule(BaseVarsPlugin):
    def get_vars(self, loader, path, entities, cache=True):
        out = {}
        for entity in entities:
            kind = "host" if isinstance(entity, Host) else "group"
            out["late_" + entity.name] = "late"
            out["clash_" + kind + "_" + entity.name] = "late"
        return out
