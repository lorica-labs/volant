# SPDX-License-Identifier: GPL-3.0-or-later
from ansible.plugins.lookup import LookupBase


class LookupModule(LookupBase):
    def run(self, terms, variables=None, **kwargs):
        out = []
        for name in terms:
            try:
                out.append(self._templar.template(variables[name]))
            except Exception as e:
                out.append(f"{type(e).__name__}: {e.message if hasattr(e, 'message') else e}")
        return out
