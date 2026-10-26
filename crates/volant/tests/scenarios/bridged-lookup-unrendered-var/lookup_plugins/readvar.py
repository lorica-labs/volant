# SPDX-License-Identifier: GPL-3.0-or-later
from ansible.plugins.lookup import LookupBase


class LookupModule(LookupBase):
    def run(self, terms, variables=None, **kwargs):
        out = []
        for name in terms:
            out.append(self._templar.template(variables[name]))
        return out
