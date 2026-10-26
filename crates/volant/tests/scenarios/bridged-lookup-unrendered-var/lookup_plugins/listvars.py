# SPDX-License-Identifier: GPL-3.0-or-later
from ansible.plugins.lookup import LookupBase


class LookupModule(LookupBase):
    def run(self, terms, variables=None, **kwargs):
        return [sorted(k for k in variables if k.startswith("my_"))]
