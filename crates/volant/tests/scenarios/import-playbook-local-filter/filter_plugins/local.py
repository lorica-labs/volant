# SPDX-License-Identifier: GPL-3.0-or-later


def suffixed(value):
    return value + "-site"


class FilterModule:
    def filters(self):
        return {"suffixed": suffixed}
