#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-3.0-or-later
import json
import sys

if sys.argv[1:] == ["--list"]:
    print(json.dumps({
        "web": {"hosts": ["web", "other"], "vars": {"g": "group"}},
        "_meta": {"hostvars": {"web": {"x": "host web var", "ansible_connection": "local"},
                               "other": {"ansible_connection": "local"}}},
    }))
else:
    print("{}")
