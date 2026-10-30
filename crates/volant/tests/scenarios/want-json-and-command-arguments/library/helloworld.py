#!/usr/bin/python
# WANT_JSON
from __future__ import annotations

import json
import sys

try:
    with open(sys.argv[1], 'r') as f:
        data = json.load(f)
except (OSError, IndexError):
    print(json.dumps(dict(msg="No argument file provided", failed=True)))
    sys.exit(1)

salutation = data.get('salutation', 'Hello')
name = data.get('name', 'World')
print(json.dumps(dict(msg='%s, %s!' % (salutation, name))))
