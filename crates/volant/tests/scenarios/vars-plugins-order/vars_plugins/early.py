from ansible.inventory.host import Host
from ansible.plugins.vars import BaseVarsPlugin


class VarsModule(BaseVarsPlugin):
    def get_vars(self, loader, path, entities, cache=True):
        out = {}
        for entity in entities:
            kind = "host" if isinstance(entity, Host) else "group"
            out["early_" + entity.name] = "early"
            out["clash_" + kind + "_" + entity.name] = "early"
        return out
