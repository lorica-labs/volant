# SPDX-License-Identifier: GPL-3.0-or-later
"""Records the goldens with the reference ansible-core: `generate.py [kind...]`, every kind when
none is named. The interpreter comes from the ansible-core tool environment, which has PyYAML.
"""
import sys

from reference import inventory, module, scenario, template
from reference.run import Failed, check_reference

# In this order when all run: `module` last, as the only kind that creates an account and a group
# on the machine.
KINDS = {"template": template, "inventory": inventory, "scenario": scenario, "module": module}


def main(names) -> int:
    unknown = [name for name in names if name not in KINDS]
    if unknown:
        print(f"no golden kind named {', '.join(unknown)}: the kinds are {', '.join(KINDS)}", file=sys.stderr)
        return 2
    try:
        check_reference()
        for name, kind in KINDS.items():
            if not names or name in names:
                kind.record()
    except Failed as e:
        print(e, file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
