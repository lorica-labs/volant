#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-3.0-or-later
import json,sys
print(json.dumps({"_meta":{"hostvars":{"h1":{"v":1},"h2":{"v":2}}},"g":{"hosts":["h1","h2"],"vars":{"gv":"g"},"children":["c"]},"c":["h3"],"ungr":{"hosts":["h4"]}}))
