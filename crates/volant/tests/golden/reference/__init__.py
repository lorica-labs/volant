# SPDX-License-Identifier: GPL-3.0-or-later
"""Records what the reference ansible-core does, for the four golden kinds.

`template` runs every row of cases.yml, `inventory` reads inventory.ini and its patterns,
`scenario` runs every `tests/scenarios/<name>/case.yml`, and `module` runs the module play and
writes `module/`. `generate.py` beside this package picks the kinds to record.
"""
import os

GOLDEN = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
SCENARIOS = os.path.normpath(os.path.join(GOLDEN, "..", "scenarios"))
with open(os.path.join(GOLDEN, "ANSIBLE_VERSION"), encoding="utf-8") as _f:
    REFERENCE = _f.read().strip()
