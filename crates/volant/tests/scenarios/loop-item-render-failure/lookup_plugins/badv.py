# SPDX-License-Identifier: GPL-3.0-or-later
from ansible.plugins.lookup import LookupBase


class LookupModule(LookupBase):
    def run(self, terms, variables=None, **kwargs):
        raise ValueError("lookup boom value")
