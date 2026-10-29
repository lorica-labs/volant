"""The cliconf of the fake device in `fake_device.py`: commands, its running configuration, and
configuration changes that report the lines they add."""

import json

from ansible.module_utils.common.text.converters import to_text
from ansible_collections.ansible.netcommon.plugins.module_utils.network.common.utils import to_list
from ansible_collections.ansible.netcommon.plugins.plugin_utils.cliconf_base import CliconfBase


class Cliconf(CliconfBase):
    def get_device_info(self):
        return {"network_os": "fake", "network_os_version": "1.0"}

    def get_config(self, source="running", flags=None, format=None):
        return self.send_command(b"show running-config")

    def edit_config(self, candidate=None, commit=True, replace=None, comment=None):
        running = to_text(self.get_config()).splitlines()
        added = [line for line in to_list(candidate) if line and line not in running]
        if added and commit:
            self.send_command(b"configure terminal")
            for line in added:
                self.send_command(line)
            self.send_command(b"end")
        response = {"request": added, "response": []}
        if added:
            response["diff"] = "\n".join("+%s" % line for line in added)
        return response

    def get(self, command=None, prompt=None, answer=None, sendonly=False, newline=True,
            output=None, check_all=False):
        return self.send_command(command=command, prompt=prompt, answer=answer,
                                 sendonly=sendonly, newline=newline, check_all=check_all)

    def get_device_operations(self):
        return {
            "supports_diff_replace": False,
            "supports_commit": False,
            "supports_rollback": False,
            "supports_defaults": False,
            "supports_onbox_diff": True,
            "supports_commit_comment": False,
            "supports_multiline_delimiter": False,
            "supports_diff_match": False,
            "supports_diff_ignore_lines": False,
            "supports_generate_diff": False,
            "supports_replace": False,
        }

    def get_capabilities(self):
        result = super().get_capabilities()
        result["device_operations"] = self.get_device_operations()
        return json.dumps(result)
