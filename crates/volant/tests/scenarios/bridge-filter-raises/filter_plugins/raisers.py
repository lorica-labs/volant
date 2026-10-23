# SPDX-License-Identifier: GPL-3.0-or-later
from ansible.errors import AnsibleError, AnsibleFilterError


class FilterModule:
    def filters(self):
        return {"filter_error": self.filter_error, "core_error": self.core_error}

    def filter_error(self, value):
        raise AnsibleFilterError("filter said no")

    def core_error(self, value):
        raise AnsibleError("core said no")
