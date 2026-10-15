# SPDX-License-Identifier: GPL-3.0-or-later
"""What a recording replaces before it is written: values that name the machine or the account
that ran the generator, or that change on every run."""
import grp
import os
import pwd

TMP = "<golden-tmp>"
HOST = "<golden-generator-host>"
MACHINE_OUTPUT = "<golden-machine-output>"
STAGED = "<golden-staged-path>"
SYSTEMD_VALUE = "<golden-systemd-value>"

# The account placeholders of a native recording: compared by value, once Volant's answer and a
# live reference answer are masked the same way.
NATIVE_ACCOUNT = {
    "owner": "<user>",
    "pw_name": "<user>",
    "group": "<group>",
    "gr_name": "<group>",
    "uid": "<uid>",
    "gid": "<gid>",
}
# The account placeholders of a recording compared by its keys and their types (`compare: shape`):
# a name stays a string and an id an integer.
SHAPE_ACCOUNT = {
    "owner": "<golden-generator-account>",
    "pw_name": "<golden-generator-account>",
    "group": "<golden-generator-account>",
    "gr_name": "<golden-generator-account>",
    "uid": 999999999,
    "gid": 999999999,
}

# Where the reference stages a file on the controller before pushing it, and the equivalent for
# `connection: local`. Random and under the account's home: any string that carries one is
# replaced, wherever it turns up (`unarchive` quotes it inside the `tar` command line it ran).
STAGED_MARKERS = ("ansible-tmp-", "/tmp/ansible", ".ansible/tmp")


def staged_paths(value):
    if isinstance(value, str):
        return STAGED if any(marker in value for marker in STAGED_MARKERS) else value
    if isinstance(value, list):
        return [staged_paths(v) for v in value]
    if isinstance(value, dict):
        return {k: staged_paths(v) for k, v in value.items()}
    return value


def in_strings(value, replacements):
    """Each `(old, new)` replaced in every string: the module directory and the machine's name,
    which `user`'s invocation carries even when no key is generated."""
    if isinstance(value, str):
        for old, new in replacements:
            value = value.replace(old, new)
        return value
    if isinstance(value, list):
        return [in_strings(v, replacements) for v in value]
    if isinstance(value, dict):
        return {k: in_strings(v, replacements) for k, v in value.items()}
    return value


def _own_account():
    me = pwd.getpwuid(os.getuid())
    group = grp.getgrgid(os.getgid()).gr_name
    return {
        "owner": me.pw_name,
        "pw_name": me.pw_name,
        "group": group,
        "gr_name": group,
        "uid": os.getuid(),
        "gid": os.getgid(),
    }


def accounts(value, placeholders, own=None):
    """Replace an ownership value only where it is the generating account's own: `root`, `0`, the
    synthetic account and anything else stay literal, so a native answering the wrong account, or
    `pw_name` where `gr_name` belongs, still differs once both sides are masked."""
    own = own or _own_account()
    if isinstance(value, list):
        return [accounts(v, placeholders, own) for v in value]
    if isinstance(value, dict):
        out = {}
        for key, v in value.items():
            mine = key in placeholders and not isinstance(v, bool) and v == own[key]
            out[key] = placeholders[key] if mine else accounts(v, placeholders, own)
        return out
    return value


def systemd_values(result):
    """`service`'s `status`, the reference's own live `systemctl show` of the unit, keeps its keys:
    every value (timestamps, counters, a per-boot id) changes between two runs."""
    status = result.get("status")
    if isinstance(status, dict):
        result["status"] = {key: SYSTEMD_VALUE for key in status}
    return result
