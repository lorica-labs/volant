# SPDX-License-Identifier: GPL-3.0-or-later
"""Records the goldens with the reference ansible-core: `generate.py [kind...]`, every kind when
none is named. `module:<case or group>` records only those module cases. The interpreter comes from the ansible-core tool environment, which has PyYAML.
"""
import sys

from reference import command_names, inventory, module, scenario, template
from reference.run import Failed, check_reference

# In this order when all run: `module` last, as the only kind that creates an account and a group
# on the machine.
KINDS = {"template": template, "inventory": inventory, "scenario": scenario, "command-names": command_names, "module": module}


def main(names) -> int:
    asked = {}
    for name in names:
        kind, _, only = name.partition(":")
        asked.setdefault(kind, [])
        if only:
            asked[kind].append(only)
    unknown = [kind for kind in asked if kind not in KINDS]
    if unknown:
        print(f"no golden kind named {', '.join(unknown)}: the kinds are {', '.join(KINDS)}", file=sys.stderr)
        return 2
    only_kinds = [kind for kind, only in asked.items() if only and kind != "module"]
    if only_kinds:
        print(f"only the module kind records single cases, not {', '.join(only_kinds)}", file=sys.stderr)
        return 2
    try:
        check_reference()
        for name, kind in KINDS.items():
            if not asked or name in asked:
                kind.record(*([asked[name]] if asked.get(name) else []))
    except Failed as e:
        print(e, file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
