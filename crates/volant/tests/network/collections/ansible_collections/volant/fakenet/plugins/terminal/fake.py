"""The terminal of the fake device in `fake_device.py`: its prompts and its error line."""

import re

from ansible_collections.ansible.netcommon.plugins.plugin_utils.terminal_base import TerminalBase


class TerminalModule(TerminalBase):
    terminal_stdout_re = [re.compile(rb"[\r\n]?fake(?:\(config\))?# ?$")]
    terminal_stderr_re = [re.compile(rb"% ?Error")]

    def on_open_shell(self):
        self._exec_cli_command(b"terminal length 0")
